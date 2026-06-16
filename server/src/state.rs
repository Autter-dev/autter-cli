//! Shared application state.

use sqlx::PgPool;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// When true, unknown credentials are auto-provisioned against a single
    /// "dev" org so the storage endpoints can be exercised without the full
    /// auth flow. Controlled by `AUTTER_SERVER_DEV_AUTH=1`. Never enable in
    /// production.
    pub dev_auth: bool,
}
