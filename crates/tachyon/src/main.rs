//! The `tachyon` binary.

mod config;
mod schema_cmd;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;

use clap::{Parser, Subcommand};
use sqlx::PgPool;
use tracing_subscriber::EnvFilter;

use config::Config;

#[derive(Parser)]
#[command(name = "tachyon", version, about = "TachyonCMS server")]
struct Cli {
    /// Config file. Without it, tachyon.toml is read if it exists. Every setting can also come
    /// from a TACHYON_* environment variable.
    #[arg(long, global = true)]
    config: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run pending migrations, then serve the API.
    Serve,
    /// Run pending migrations and exit.
    Migrate,
    /// Keep collection schemas in a JSON file: export, compare, apply.
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Write every collection's schema as JSON.
    Export {
        /// File to write. Prints to stdout when omitted.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Show what applying a schema file would change, without changing anything.
    Diff {
        file: PathBuf,
        /// Rename a field instead of removing it and adding a new one: collection:field=new_name
        #[arg(long = "rename")]
        renames: Vec<String>,
    },
    /// Apply a schema file. Changes that delete or retype data need --yes.
    Apply {
        file: PathBuf,
        /// Rename a field instead of removing it and adding a new one: collection:field=new_name
        #[arg(long = "rename")]
        renames: Vec<String>,
        /// Go ahead with changes that delete or retype data.
        #[arg(long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    // A missing schema file or a bad --rename should say so at once, not after the database
    // answers.
    let schema_input = match &cli.command {
        Command::Schema {
            command:
                SchemaCommand::Diff { file, renames } | SchemaCommand::Apply { file, renames, .. },
        } => Some((read_schema(file)?, schema_cmd::parse_renames(renames)?)),
        _ => None,
    };
    let pool = PgPool::connect(&config.database_url)
        .await
        .with_context(|| {
            format!(
                "couldn't connect to Postgres at {}; for local development, start it with `docker compose up -d`",
                database_location(&config.database_url)
            )
        })?;
    tachyon_db::MIGRATOR.run(&pool).await?;

    match cli.command {
        Command::Serve => {
            config.check_serve().map_err(anyhow::Error::msg)?;
            let jobs = config
                .jobs_config(pool.options().get_max_connections())
                .map_err(anyhow::Error::msg)?;
            let running = jobs.running.clone();
            let options = tachyon_api::Options {
                auth: config.auth_config().map_err(anyhow::Error::msg)?,
                jobs,
                schema_inline_rows: None,
            };
            let app = tachyon_api::router_with(pool.clone(), options).await?;
            let listener = tokio::net::TcpListener::bind(config.listen).await?;
            tracing::info!(addr = %config.listen, "listening");
            if let Some(warning) =
                config::setup_warning(tachyon_db::auth::has_accounts(&pool).await?)
            {
                tracing::warn!("{warning}");
            }
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown())
                .await?;
            // Gives the job workers a while to finish what they're running; they stop with the
            // pool. Cutting one off is safe: its transaction rolls back, its lease runs out,
            // and the run that takes it over is the only one that can complete it.
            if tokio::time::timeout(Duration::from_secs(30), pool.close())
                .await
                .is_err()
            {
                tracing::warn!(
                    jobs = ?running.ids(),
                    "stopped with jobs still running; they run again once their lease runs out"
                );
            }
        }
        Command::Migrate => {}
        Command::Schema { command } => match command {
            SchemaCommand::Export { out } => {
                let json = schema_cmd::export(&pool).await?;
                match out {
                    Some(path) => std::fs::write(&path, json)
                        .with_context(|| format!("writing {}", path.display()))?,
                    None => print!("{json}"),
                }
            }
            SchemaCommand::Diff { .. } => {
                let (text, renames) = schema_input.expect("read before connecting");
                println!("{}", schema_cmd::run(&pool, &text, &renames, None).await?);
            }
            SchemaCommand::Apply { yes, .. } => {
                let (text, renames) = schema_input.expect("read before connecting");
                println!(
                    "{}",
                    schema_cmd::run(&pool, &text, &renames, Some(yes)).await?
                );
            }
        },
    }
    Ok(())
}

fn read_schema(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// Where `database_url` points, without the user or password: `postgres://u:secret@h:5432/db`
/// is `h:5432/db`. It goes into error messages, so it must never carry the password.
fn database_location(database_url: &str) -> String {
    // Anything else, such as `u:secret@h/db`, may parse as some other kind of URL with the
    // password in it, so it's described rather than shown.
    let Some(url) = url::Url::parse(database_url)
        .ok()
        .filter(|u| matches!(u.scheme(), "postgres" | "postgresql"))
    else {
        return "database_url, which isn't a postgres:// URL".into();
    };
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .unwrap_or("localhost");
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!("{host}{port}{}", url.path().trim_end_matches('/'))
}

async fn shutdown() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{database_location, read_schema};

    #[test]
    fn database_location_leaves_out_the_user_and_password() {
        for (url, location) in [
            ("postgres://u:secret@h:5432/db", "h:5432/db"),
            (
                "postgres://tachyon:tachyon@127.0.0.1:54329/tachyon",
                "127.0.0.1:54329/tachyon",
            ),
            ("postgres://u:secret@h/db", "h/db"),
            ("postgres://u:secret@h:5432", "h:5432"),
            (
                "postgresql://u@[::1]:5432/db?sslmode=require",
                "[::1]:5432/db",
            ),
            ("postgres:///db", "localhost/db"),
            (
                "u:secret@h/db",
                "database_url, which isn't a postgres:// URL",
            ),
            (
                "not a url secret",
                "database_url, which isn't a postgres:// URL",
            ),
        ] {
            let got = database_location(url);
            assert_eq!(got, location, "{url}");
            assert!(!got.contains("secret"), "{url}");
        }
    }

    #[test]
    fn a_missing_schema_file_is_named() {
        let err = read_schema("nope.json".as_ref()).unwrap_err();
        assert!(
            format!("{err:#}").starts_with("reading nope.json: "),
            "{err:#}"
        );
    }
}
