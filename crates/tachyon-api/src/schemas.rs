//! Compiled collection schemas, held in memory so requests never wait on a lookup.
//! Every server reloads when any server changes a schema, via Postgres LISTEN/NOTIFY.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tachyon_db::DbError;
use tachyon_db::collections::{self, Collection};
use uuid::Uuid;

pub const CHANNEL: &str = "tachyon_schema";

pub struct Schemas {
    by_slug: ArcSwap<HashMap<String, Arc<Collection>>>,
}

impl Schemas {
    pub async fn load(pool: &PgPool) -> Result<Self, DbError> {
        let schemas = Self {
            by_slug: ArcSwap::from_pointee(HashMap::new()),
        };
        schemas.reload(pool).await?;
        Ok(schemas)
    }

    // NOTE: reloads every schema on any change; reload per collection if there are thousands.
    pub async fn reload(&self, pool: &PgPool) -> Result<(), DbError> {
        let all = collections::list(pool).await?;
        let map = all
            .into_iter()
            .map(|c| (c.schema.slug.clone(), Arc::new(c)))
            .collect();
        self.by_slug.store(Arc::new(map));
        Ok(())
    }

    pub fn get(&self, slug: &str) -> Option<Arc<Collection>> {
        self.by_slug.load().get(slug).cloned()
    }

    pub fn get_by_id(&self, id: Uuid) -> Option<Arc<Collection>> {
        self.by_slug.load().values().find(|c| c.id == id).cloned()
    }
}

/// Keeps `schemas` current until the pool closes. Reconnects after errors.
pub fn spawn_listener(pool: PgPool, schemas: Arc<Schemas>) {
    tokio::spawn(async move {
        while !pool.is_closed() {
            if let Err(error) = listen(&pool, &schemas).await {
                if pool.is_closed() {
                    break;
                }
                tracing::warn!(%error, "schema listener failed; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
}

async fn listen(pool: &PgPool, schemas: &Schemas) -> Result<(), DbError> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    // Catch anything that changed while we were (re)connecting.
    schemas.reload(pool).await?;
    loop {
        // `None` means the connection dropped. Start over with a fresh listener, which
        // re-subscribes and reloads, so nothing announced while we were away is missed.
        if listener.try_recv().await?.is_none() {
            return Ok(());
        }
        schemas.reload(pool).await?;
    }
}
