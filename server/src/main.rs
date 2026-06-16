//! Autter platform backend (data plane).
//!
//! Receives authorship-note and prompt (CAS) uploads from the autter CLI and
//! persists them to each organization's own PostgreSQL database. Identity comes
//! from a Better Auth JWT issued by autter.dev (verified against its JWKS); the
//! token's `org_db_url` claim tells us which database to write to.
//!
//! This is the open-source record of exactly what the platform stores — see
//! `migrations/0001_init.sql`.
//!
//! Configuration (environment):
//! - `BIND`                     listen address (default 0.0.0.0:8787)
//! - `AUTTER_SERVER_JWKS_URL`   Better Auth JWKS endpoint, e.g.
//!                              https://autter.dev/api/auth/jwks (required unless dev)
//! - `AUTTER_SERVER_JWT_ISSUER`   optional expected `iss`
//! - `AUTTER_SERVER_JWT_AUDIENCE` optional expected `aud`
//! - `AUTTER_SERVER_DEV_AUTH`   set to `1` to decode tokens WITHOUT verifying
//!                              the signature (development only).

mod auth;
mod error;
mod models;
mod routes;
mod state;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use sqlx::migrate::Migrator;
use tracing_subscriber::EnvFilter;

use crate::auth::JwtVerifier;
use crate::state::{AppState, PoolManager};

/// Per-org database schema, applied to each org database on first connect.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8787".to_string());
    let dev_auth = std::env::var("AUTTER_SERVER_DEV_AUTH").as_deref() == Ok("1");
    let jwks_url = non_empty_env("AUTTER_SERVER_JWKS_URL");
    let issuer = non_empty_env("AUTTER_SERVER_JWT_ISSUER");
    let audience = non_empty_env("AUTTER_SERVER_JWT_AUDIENCE");

    if dev_auth {
        tracing::warn!(
            "AUTTER_SERVER_DEV_AUTH is on: JWT signatures are NOT verified. Do not use in production."
        );
    } else if jwks_url.is_none() {
        anyhow::bail!(
            "AUTTER_SERVER_JWKS_URL must be set (e.g. https://autter.dev/api/auth/jwks), or set AUTTER_SERVER_DEV_AUTH=1 for local development"
        );
    }

    let verifier = JwtVerifier::new(jwks_url, issuer, audience, dev_auth);

    let state = AppState {
        pools: Arc::new(PoolManager::default()),
        verifier: Arc::new(verifier),
    };

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

/// Read an env var, treating empty/whitespace as unset.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
