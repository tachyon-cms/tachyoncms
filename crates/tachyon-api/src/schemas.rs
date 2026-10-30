//! Compiled collection schemas, held in memory so requests never wait on a lookup.
//! Every server reloads when any server changes a schema, via Postgres LISTEN/NOTIFY.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use sqlx::PgPool;
use sqlx::postgres::{PgListener, PgPoolOptions};
use tachyon_core::{CollectionKind, CollectionSchema, SchemaError};
use tachyon_db::DbError;
use tachyon_db::collections::{self, Collection};
use uuid::Uuid;

pub const CHANNEL: &str = "tachyon_schema";

pub struct Schemas {
    current: ArcSwap<Snapshot>,
    /// One reload at a time, so a slow one can't store an older read over a newer one.
    reloading: tokio::sync::Mutex<()>,
}

/// Collections and globals with their shared blocks expanded, so code past the cache never
/// sees a reference. Shared blocks have no documents, so only `raw` holds them.
#[derive(Default)]
struct Snapshot {
    by_slug: HashMap<String, Arc<Collection>>,
    by_id: HashMap<Uuid, Arc<Collection>>,
    /// Every schema as stored, shared blocks included.
    raw: HashMap<String, Arc<Collection>>,
}

/// `c` with its references swapped for the definitions in `raw`, or `c` itself when it has
/// none. A reference that finds nothing (saving refuses that) stays, and its rows are
/// treated as a type the schema doesn't know.
fn expand_in(raw: &HashMap<String, Arc<Collection>>, c: &Arc<Collection>) -> Arc<Collection> {
    if c.schema.block_refs().is_empty() {
        return c.clone();
    }
    match c
        .schema
        .expand(|slug: &str| raw.get(slug).map(|b| &b.schema.0))
    {
        Ok(schema) => Arc::new(Collection {
            schema: sqlx::types::Json(schema),
            ..(**c).clone()
        }),
        Err(error) => {
            tracing::warn!(%error, collection = %c.schema.slug, "couldn't expand shared blocks");
            c.clone()
        }
    }
}

impl Schemas {
    pub async fn load(pool: &PgPool) -> Result<Self, DbError> {
        let schemas = Self {
            current: ArcSwap::from_pointee(Snapshot::default()),
            reloading: tokio::sync::Mutex::new(()),
        };
        schemas.reload(pool).await?;
        Ok(schemas)
    }

    // NOTE: reloads every schema on any change; reload per collection if there are thousands.
    pub async fn reload(&self, pool: &PgPool) -> Result<(), DbError> {
        self.reload_then(pool, async {}).await
    }

    /// `reload`, running `before_store` between reading and storing (a seam for tests).
    async fn reload_then(
        &self,
        pool: &PgPool,
        before_store: impl Future<Output = ()>,
    ) -> Result<(), DbError> {
        let _one_at_a_time = self.reloading.lock().await;
        let mut next = Snapshot::default();
        for c in collections::list(pool).await? {
            next.raw.insert(c.schema.slug.clone(), Arc::new(c));
        }
        for c in next.raw.values() {
            if c.schema.kind == CollectionKind::Block {
                continue;
            }
            let c = expand_in(&next.raw, c);
            next.by_id.insert(c.id, c.clone());
            next.by_slug.insert(c.schema.slug.clone(), c);
        }
        before_store.await;
        self.current.store(Arc::new(next));
        Ok(())
    }

    /// A collection or global by slug, expanded. Shared blocks aren't found here.
    pub fn get(&self, slug: &str) -> Option<Arc<Collection>> {
        self.current.load().by_slug.get(slug).cloned()
    }

    /// Every collection and global, expanded, as it would be with `changed` saved: one of
    /// the same slug is replaced, and a shared block's new definition reaches every schema
    /// naming it. `changed` itself is first in the list, expanded.
    /// Refuses when `changed` names a shared block that doesn't exist.
    // ponytail: expands every schema per schema save; fine for admin-rate saves, keep the
    // expanded snapshot and redo only the schemas naming `changed` if that ever shows.
    pub fn with_saved(
        &self,
        changed: &CollectionSchema,
    ) -> Result<Vec<CollectionSchema>, SchemaError> {
        let snapshot = self.current.load();
        let raw = |slug: &str| {
            if slug == changed.slug {
                Some(changed)
            } else {
                snapshot.raw.get(slug).map(|c| &c.schema.0)
            }
        };
        // A shared block too, so one naming itself through others is refused here.
        let mut out = vec![changed.expand(raw)?];
        for c in snapshot.raw.values() {
            if c.schema.slug != changed.slug && c.schema.kind != CollectionKind::Block {
                // Stored references were checked when stored, and blocks naming one can't go.
                out.push(c.schema.expand(raw).unwrap_or_else(|_| c.schema.0.clone()));
            }
        }
        Ok(out)
    }

    /// A collection's schema by slug, for filters that reach through relationships. Globals
    /// aren't linkable, so they're left out.
    // ponytail: clones the schema per relationship a filter crosses; borrow from the
    // snapshot if that shows up in a profile.
    pub fn lookup(&self, slug: &str) -> Option<CollectionSchema> {
        self.get(slug)
            .filter(|c| c.schema.kind == CollectionKind::Collection)
            .map(|c| c.schema.0.clone())
    }

    /// Every collection and global, in no particular order. Shared blocks aren't listed.
    pub fn all(&self) -> Vec<Arc<Collection>> {
        self.current.load().by_id.values().cloned().collect()
    }

    pub fn get_by_id(&self, id: Uuid) -> Option<Arc<Collection>> {
        self.current.load().by_id.get(&id).cloned()
    }
}

/// Keeps `schemas` current until the pool closes. Reconnects after errors. Listens on a
/// connection of its own, so requests keep every pooled connection.
pub fn spawn_listener(pool: PgPool, schemas: Arc<Schemas>) {
    tokio::spawn(async move {
        let own = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with((*pool.connect_options()).clone());
        while !pool.is_closed() {
            if let Err(error) = listen(&pool, &own, &schemas).await {
                if pool.is_closed() {
                    break;
                }
                tracing::warn!(%error, "schema listener failed; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        own.close().await;
    });
}

async fn listen(pool: &PgPool, own: &PgPool, schemas: &Schemas) -> Result<(), DbError> {
    let mut listener = PgListener::connect_with(own).await?;
    listener.listen(CHANNEL).await?;
    // Catch anything that changed while we were (re)connecting.
    schemas.reload(pool).await?;
    loop {
        // `None` means the connection dropped. Start over with a fresh listener, which
        // re-subscribes and reloads, so nothing announced while we were away is missed.
        tokio::select! {
            received = listener.try_recv() => {
                if received?.is_none() {
                    return Ok(());
                }
            }
            () = pool.close_event() => return Ok(()),
        }
        schemas.reload(pool).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn posts(pool: &PgPool) -> Collection {
        let schema: CollectionSchema =
            serde_json::from_value(json!({"slug": "posts", "fields": []})).unwrap();
        collections::create(pool, &schema).await.unwrap()
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_slow_reload_never_overwrites_a_newer_one(pool: PgPool) {
        let created = posts(&pool).await;
        let schemas = Arc::new(Schemas::load(&pool).await.unwrap());

        // Reload A reads version 1, then stalls before storing it.
        let (release, stalled) = tokio::sync::oneshot::channel::<()>();
        let a = {
            let (schemas, pool) = (schemas.clone(), pool.clone());
            tokio::spawn(async move {
                schemas
                    .reload_then(&pool, async {
                        stalled.await.ok();
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Meanwhile the schema moves on, and reload B starts after that.
        sqlx::query("UPDATE collections SET schema_version = 2 WHERE id = $1")
            .bind(created.id)
            .execute(&pool)
            .await
            .unwrap();
        let b = {
            let (schemas, pool) = (schemas.clone(), pool.clone());
            tokio::spawn(async move { schemas.reload(&pool).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        release.send(()).unwrap();
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();

        assert_eq!(schemas.get("posts").unwrap().schema_version, 2);
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn finds_collections_by_id(pool: PgPool) {
        let created = posts(&pool).await;
        let schemas = Schemas::load(&pool).await.unwrap();
        assert_eq!(schemas.get_by_id(created.id).unwrap().schema.slug, "posts");
        assert!(schemas.get_by_id(Uuid::now_v7()).is_none());
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn the_listener_leaves_the_pool_alone(pool: PgPool) {
        let small = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(1))
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap();
        let schemas = Arc::new(Schemas::load(&small).await.unwrap());
        spawn_listener(small.clone(), schemas.clone());
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (a, b) = tokio::join!(small.acquire(), small.acquire());
        assert!(
            a.is_ok() && b.is_ok(),
            "the listener took a pooled connection"
        );
        drop((a, b));

        // It still hears changes.
        posts(&small).await;
        for _ in 0..50 {
            if schemas.get("posts").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(schemas.get("posts").is_some());
        small.close().await;
    }
}
