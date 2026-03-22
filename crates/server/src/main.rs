mod config;
mod error;
mod routes;
mod state;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use codixing_core::Engine;

use config::ServerConfig;
use state::new_state;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cfg = ServerConfig::parse();

    let root = cfg
        .root_path
        .canonicalize()
        .with_context(|| format!("path not found: {}", cfg.root_path.display()))?;

    info!(root = %root.display(), "opening index");

    let engine = Engine::open(&root).with_context(|| {
        format!(
            "failed to open index at {} — run `codixing init` first",
            root.display()
        )
    })?;

    let state = new_state(engine);
    let router = routes::build_router(state);

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind to {addr}"))?;

    info!("Codixing server listening on http://{addr}");

    axum::serve(listener, router)
        .await
        .context("server error")?;

    Ok(())
}
