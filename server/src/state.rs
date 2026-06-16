//! Shared application state: per-org database pools + the JWT verifier.

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::RwLock;

use crate::auth::JwtVerifier;
use crate::error::AppError;

/// Lazily-created, cached connection pools keyed by org database URL.
///
/// Each organization has its own PostgreSQL database (the `org_db_url` from the
/// verified JWT). The first time we see an org's URL we open a pool and run the
/// per-org migrations against it; subsequent requests reuse the pool.
#[derive(Default)]
pub struct PoolManager {
    pools: RwLock<HashMap<String, PgPool>>,
}

impl PoolManager {
    /// Get (or lazily create + migrate) the pool for an org database URL.
    pub async fn get(&self, db_url: &str) -> Result<PgPool, AppError> {
        if let Some(pool) = self.pools.read().await.get(db_url) {
            return Ok(pool.clone());
        }

        // Upgrade to a write lock and re-check (another task may have created it).
        let mut pools = self.pools.write().await;
        if let Some(pool) = pools.get(db_url) {
            return Ok(pool.clone());
        }

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(db_url)
            .await
            .map_err(|e| AppError::Internal(format!("connect to org database failed: {e}")))?;

        // Ensure the org database has the schema. Idempotent.
        crate::MIGRATOR
            .run(&pool)
            .await
            .map_err(|e| AppError::Internal(format!("org database migration failed: {e}")))?;

        pools.insert(db_url.to_string(), pool.clone());
        tracing::info!("opened pool for a new org database");
        Ok(pool)
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pools: Arc<PoolManager>,
    pub verifier: Arc<JwtVerifier>,
}
