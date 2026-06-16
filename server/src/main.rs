//! Autter platform backend.
//!
//! Receives authorship-note and prompt (CAS) uploads from the autter CLI and
//! persists them to PostgreSQL. This is the open-source record of exactly what
//! the platform stores.
//!
//! Configuration (environment):
//! - `DATABASE_URL`         (required) e.g. postgres://autter:autter@localhost:5432/autter
//! - `BIND`                 (default 0.0.0.0:8787) listen address
//! - `AUTTER_SERVER_DEV_AUTH` set to `1` to auto-provision a dev org for any
//!                          credential (development only).

mod auth;
mod error;
mod models;
mod routes;
mod state;

use std::net::SocketAddr;

use anyhow::Context;
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL must be set (e.g. postgres://autter:autter@localhost:5432/autter)")?;

    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8787".to_string());
    let dev_auth = std::env::var("AUTTER_SERVER_DEV_AUTH").as_deref() == Ok("1");

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .context("failed to connect to PostgreSQL")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("failed to run database migrations")?;

    if dev_auth {
        tracing::warn!(
            "AUTTER_SERVER_DEV_AUTH is on: unknown credentials are auto-provisioned to a 'dev' org. Do not use in production."
        );
    }

    let state = AppState { pool, dev_auth };
    let app = routes::router(state);

    let addr: SocketAddr = bind.parse().context("invalid BIND address")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    tracing::info!("autter-server listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
