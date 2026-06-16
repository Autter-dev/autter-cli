//! Append-only CLI activity logging into each org database's `cli_audit_log`.
//!
//! Every time a device pushes new data to the cloud with a Personal Access Token
//! (authorship notes on a commit, prompt/transcript CAS objects), we record a
//! `data.push` event — attributed to the user and, when present, the specific PAT
//! (`token_id`, carried as a JWT claim). The companion `pat.created` / `pat.signin`
//! events are written by the control plane.
//!
//! The table is shared with the control plane (`bootstrap-org-tables`); a worker
//! migration also creates it `IF NOT EXISTS` so the worker is self-contained.
//!
//! Audit writes are best-effort: a failure is logged but never fails the request.

use sqlx::PgPool;

use crate::auth::Identity;

/// Record a `data.push` event into the org's `cli_audit_log`. `resource_id` is
/// optional (e.g. a CAS batch has no single resource). Never returns an error.
pub async fn record_push(
    pool: &PgPool,
    identity: &Identity,
    resource_id: Option<&str>,
    detail: serde_json::Value,
) {
    // `id` is generated in SQL (gen_random_uuid() is built into Postgres 13+);
    // `created_at` defaults to now() in the schema.
    let result = sqlx::query(
        "INSERT INTO cli_audit_log
            (id, event_type, actor_id, actor_email, token_id, resource_id, detail)
         VALUES (gen_random_uuid()::text, 'data.push', $1, $2, $3, $4, $5)",
    )
    .bind(identity.user_id.as_deref())
    .bind(identity.email.as_deref())
    .bind(identity.token_id.as_deref())
    .bind(resource_id)
    .bind(&detail)
    .execute(pool)
    .await;

    if let Err(e) = result {
        tracing::warn!("cli audit write failed: {e}");
    }
}
