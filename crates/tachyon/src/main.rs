//! The `tachyon` binary.

mod config;

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let pool = PgPool::connect(&config.database_url).await?;
    tachyon_db::MIGRATOR.run(&pool).await?;

    if let Command::Serve = cli.command {
        let listener = tokio::net::TcpListener::bind(config.listen).await?;
        tracing::info!(addr = %config.listen, "listening");
        axum::serve(listener, tachyon_api::router(pool))
            .with_graceful_shutdown(shutdown())
            .await?;
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
