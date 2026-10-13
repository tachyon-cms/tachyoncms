//! The `tachyon` binary.

mod config;
mod schema_cmd;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use sqlx::PgPool;
use tracing_subscriber::EnvFilter;

use config::Config;

#[derive(Parser)]
#[command(name = "tachyon", version, about = "TachyonCMS server")]
struct Cli {
    /// Config file. Every setting can also come from a TACHYON_* environment variable.
    #[arg(long, global = true, default_value = "tachyon.toml")]
    config: String,
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
    let config = Config::load(&cli.config)?;
    let pool = PgPool::connect(&config.database_url).await?;
    tachyon_db::MIGRATOR.run(&pool).await?;

    match cli.command {
        Command::Serve => {
            config.check_serve().map_err(anyhow::Error::msg)?;
            let options = tachyon_api::Options {
                auth: config.auth_config().map_err(anyhow::Error::msg)?,
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
        }
        Command::Migrate => {}
        Command::Schema { command } => match command {
            SchemaCommand::Export { out } => {
                let json = schema_cmd::export(&pool).await?;
                match out {
                    Some(path) => std::fs::write(path, json)?,
                    None => print!("{json}"),
                }
            }
            SchemaCommand::Diff { file, renames } => {
                let text = std::fs::read_to_string(file)?;
                let renames = schema_cmd::parse_renames(&renames)?;
                println!("{}", schema_cmd::run(&pool, &text, &renames, None).await?);
            }
            SchemaCommand::Apply { file, renames, yes } => {
                let text = std::fs::read_to_string(file)?;
                let renames = schema_cmd::parse_renames(&renames)?;
                println!(
                    "{}",
                    schema_cmd::run(&pool, &text, &renames, Some(yes)).await?
                );
            }
        },
    }
    Ok(())
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
