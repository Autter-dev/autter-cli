//! Request authentication.
//!
//! The CLI authenticates uploads with either an OAuth bearer token
//! (`Authorization: Bearer <token>`) or an API key (`X-API-Key: <key>`), and
//! always sends `X-Distinct-ID`. Here we resolve that credential to an org via
//! the `access_tokens` table.
//!
//! Full OAuth token issuance is a separate batch; for the storage-first cut,
//! `AUTTER_SERVER_DEV_AUTH=1` auto-provisions a single "dev" org for any
//! previously-unseen credential so the pipeline can be tested end-to-end.

use axum::http::HeaderMap;
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

/// The authenticated caller for a request.
pub struct AuthOrg {
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub distinct_id: Option<String>,
}

/// Pull the credential the CLI presents out of the request headers.
fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        let key = key.trim();
        if !key.is_empty() {
            return Some(key.to_string());
        }
    }

    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(rest) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }

    None
}

/// Resolve the caller's org from the presented credential.
pub async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<AuthOrg, AppError> {
    let distinct_id = headers
        .get("x-distinct-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let token = extract_token(headers)
        .ok_or_else(|| AppError::Unauthorized("Missing credentials".to_string()))?;

    // Known credential → use its org/user.
    if let Some(row) =
        sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT org_id, user_id FROM access_tokens WHERE token = $1",
        )
        .bind(&token)
        .fetch_optional(&state.pool)
        .await?
    {
        return Ok(AuthOrg {
            org_id: row.0,
            user_id: row.1,
            distinct_id,
        });
    }

    // Unknown credential.
    if state.dev_auth {
        let org_id = ensure_dev_org(state).await?;
        sqlx::query(
            "INSERT INTO access_tokens (token, org_id) VALUES ($1, $2)
             ON CONFLICT (token) DO NOTHING",
        )
        .bind(&token)
        .bind(org_id)
        .execute(&state.pool)
        .await?;

        return Ok(AuthOrg {
            org_id,
            user_id: None,
            distinct_id,
        });
    }

    Err(AppError::Unauthorized("Invalid credentials".to_string()))
}

/// Ensure the single "dev" org exists and return its id (dev mode only).
async fn ensure_dev_org(state: &AppState) -> Result<Uuid, AppError> {
    sqlx::query(
        "INSERT INTO orgs (slug, name) VALUES ('dev', 'Dev Org')
         ON CONFLICT (slug) DO NOTHING",
    )
    .execute(&state.pool)
    .await?;

    let (id,): (Uuid,) = sqlx::query_as("SELECT id FROM orgs WHERE slug = 'dev'")
        .fetch_one(&state.pool)
        .await?;

    Ok(id)
}
