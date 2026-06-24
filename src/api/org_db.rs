//! Direct writes to an organization's own PostgreSQL database.
//!
//! The autter CLI used to POST authorship notes and prompt-transcript (CAS)
//! objects to a hosted data-plane backend, which re-decoded the caller's JWT and
//! connected to the org database on the CLI's behalf. That backend was a pure
//! pass-through: the access token the CLI already holds carries the org's
//! database URL in its `org_db_url` claim. So we cut out the middle tier — the
//! CLI reads `org_db_url` straight from its token and writes to the org database
//! itself, using the machine's own resources.
//!
//! Identity (who uploaded what) comes from the same token: `sub` (user id),
//! `email`, and `token_id` (the PAT, when present). Because every database
//! belongs to exactly one org, rows are not org-scoped — the database boundary
//! IS the tenant boundary, exactly as the old backend's schema documented.
//!
//! Connections are cached per `org_db_url` for the life of the process (the
//! daemon is long-lived, so this avoids a TLS handshake on every flush). A query
//! that fails on a dropped connection transparently reconnects once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use once_cell::sync::Lazy;
use postgres::Client;
use sha2::{Digest, Sha256};

use crate::api::types::{
    CAPromptStoreReadResponse, CAPromptStoreReadResult, CasObject, CasUploadResponse,
    CasUploadResult, NoteEntry, NotesReadResponse, NotesUploadResponse,
};
use crate::error::AutterError;
use crate::metrics::MetricEvent;

/// Identity + routing decoded from the caller's access-token JWT.
#[derive(Debug, Clone)]
pub struct OrgIdentity {
    /// Postgres connection URL for the org's database (`org_db_url` claim).
    pub org_db_url: String,
    /// Better Auth user id (`sub`), recorded as `uploaded_by`.
    pub user_id: Option<String>,
    /// User email, recorded in audit rows.
    pub email: Option<String>,
    /// PAT id (`token_id`), recorded in audit rows when the session used a PAT.
    pub token_id: Option<String>,
}

/// Decode the (unverified) payload of a JWT and pull out the org routing claims.
///
/// The token is the CLI's own access token — it was minted and signed by
/// autter.dev and verified there at issue time, so we only need to *read* its
/// claims here, not re-verify the signature.
pub fn identity_from_token(token: &str) -> Result<OrgIdentity, AutterError> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| AutterError::Generic("access token is not a well-formed JWT".to_string()))?;

    // JWT uses base64url without padding; tolerate padding just in case.
    let trimmed = payload.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .map_err(|e| AutterError::Generic(format!("failed to decode token payload: {e}")))?;
    let claims: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(AutterError::JsonError)?;

    let org_db_url = claims
        .get("org_db_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AutterError::Generic("token missing org_db_url claim".to_string()))?
        .to_string();

    let str_claim = |key: &str| {
        claims
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };

    Ok(OrgIdentity {
        org_db_url,
        user_id: str_claim("sub"),
        email: str_claim("email"),
        token_id: str_claim("token_id"),
    })
}

/// Process-wide cache: `org_db_url` → live Postgres client.
static CONNECTIONS: Lazy<Mutex<HashMap<String, Arc<Mutex<Client>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Per-org schema, created on first connect (idempotent). Mirrors the old
/// backend's `migrations/0001_init.sql` and `0002_cli_audit_log.sql` so a fresh
/// org database is fully provisioned by the CLI alone. autter.dev's
/// `bootstrap-org-tables` may also create these; `IF NOT EXISTS` keeps both safe.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS authorship_notes (
    commit_sha  TEXT PRIMARY KEY,
    content     TEXT NOT NULL,
    repo_url    TEXT,
    uploaded_by TEXT,
    distinct_id TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE authorship_notes ADD COLUMN IF NOT EXISTS repo_url TEXT;
CREATE TABLE IF NOT EXISTS cas_objects (
    hash        TEXT PRIMARY KEY,
    content     JSONB NOT NULL,
    metadata    JSONB NOT NULL DEFAULT '{}'::jsonb,
    uploaded_by TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS cli_audit_log (
    id           TEXT PRIMARY KEY,
    event_type   TEXT NOT NULL,
    actor_id     TEXT,
    actor_email  TEXT,
    token_id     TEXT,
    token_name   TEXT,
    resource_id  TEXT,
    detail       JSONB,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS cli_audit_log_eventType_idx ON cli_audit_log (event_type);
CREATE INDEX IF NOT EXISTS cli_audit_log_tokenId_idx ON cli_audit_log (token_id);
CREATE INDEX IF NOT EXISTS cli_audit_log_createdAt_idx ON cli_audit_log (created_at);
CREATE TABLE IF NOT EXISTS cli_metrics (
    id            BIGSERIAL PRIMARY KEY,
    event_id      INTEGER NOT NULL,
    event_ts      TIMESTAMPTZ NOT NULL,
    event_values  JSONB NOT NULL DEFAULT '{}'::jsonb,
    event_attrs   JSONB NOT NULL DEFAULT '{}'::jsonb,
    uploaded_by   TEXT,
    distinct_id   TEXT,
    dedup_key     TEXT UNIQUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS cli_metrics_eventId_idx ON cli_metrics (event_id);
CREATE INDEX IF NOT EXISTS cli_metrics_eventTs_idx ON cli_metrics (event_ts);
CREATE TABLE IF NOT EXISTS file_change_counts (
    repo_url        TEXT NOT NULL,
    file_path       TEXT NOT NULL,
    change_count    BIGINT NOT NULL DEFAULT 0,
    lines_added     BIGINT NOT NULL DEFAULT 0,
    lines_deleted   BIGINT NOT NULL DEFAULT 0,
    last_changed_at TIMESTAMPTZ NOT NULL,
    distinct_id     TEXT,
    uploaded_by     TEXT,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repo_url, file_path)
);
CREATE INDEX IF NOT EXISTS file_change_counts_repo_idx ON file_change_counts (repo_url);
CREATE INDEX IF NOT EXISTS file_change_counts_count_idx ON file_change_counts (repo_url, change_count DESC);
CREATE TABLE IF NOT EXISTS commit_authorship_summaries (
    commit_sha             TEXT PRIMARY KEY,
    repo_url               TEXT,
    branch                 TEXT,
    base_commit_sha        TEXT,
    human_author           TEXT,
    git_diff_added_lines   BIGINT NOT NULL DEFAULT 0,
    git_diff_deleted_lines BIGINT NOT NULL DEFAULT 0,
    human_additions        BIGINT NOT NULL DEFAULT 0,
    ai_additions           BIGINT NOT NULL DEFAULT 0,
    ai_accepted            BIGINT NOT NULL DEFAULT 0,
    unknown_additions      BIGINT NOT NULL DEFAULT 0,
    human_percent          DOUBLE PRECISION NOT NULL DEFAULT 0,
    ai_percent             DOUBLE PRECISION NOT NULL DEFAULT 0,
    unknown_percent        DOUBLE PRECISION NOT NULL DEFAULT 0,
    tool_model_breakdown   JSONB NOT NULL DEFAULT '{}'::jsonb,
    prompts                JSONB NOT NULL DEFAULT '[]'::jsonb,
    hunks                  JSONB NOT NULL DEFAULT '[]'::jsonb,
    distinct_id            TEXT,
    uploaded_by            TEXT,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS repo_url TEXT;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS branch TEXT;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS base_commit_sha TEXT;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS human_author TEXT;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS git_diff_added_lines BIGINT NOT NULL DEFAULT 0;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS git_diff_deleted_lines BIGINT NOT NULL DEFAULT 0;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS human_additions BIGINT NOT NULL DEFAULT 0;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS ai_additions BIGINT NOT NULL DEFAULT 0;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS ai_accepted BIGINT NOT NULL DEFAULT 0;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS unknown_additions BIGINT NOT NULL DEFAULT 0;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS human_percent DOUBLE PRECISION NOT NULL DEFAULT 0;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS ai_percent DOUBLE PRECISION NOT NULL DEFAULT 0;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS unknown_percent DOUBLE PRECISION NOT NULL DEFAULT 0;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS tool_model_breakdown JSONB NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS prompts JSONB NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS hunks JSONB NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS distinct_id TEXT;
ALTER TABLE commit_authorship_summaries ADD COLUMN IF NOT EXISTS uploaded_by TEXT;
CREATE INDEX IF NOT EXISTS commit_authorship_summaries_repo_idx ON commit_authorship_summaries (repo_url);
CREATE INDEX IF NOT EXISTS commit_authorship_summaries_updated_idx ON commit_authorship_summaries (updated_at);";

/// Open a new TLS connection to `org_db_url` and ensure the schema exists.
fn connect(org_db_url: &str) -> Result<Client, AutterError> {
    let tls = native_tls::TlsConnector::new()
        .map_err(|e| AutterError::Generic(format!("failed to build TLS connector: {e}")))?;
    let connector = postgres_native_tls::MakeTlsConnector::new(tls);
    let mut client = Client::connect(org_db_url, connector)
        .map_err(|e| AutterError::Generic(format!("failed to connect to org database: {e}")))?;
    client
        .batch_execute(SCHEMA)
        .map_err(|e| AutterError::Generic(format!("failed to ensure org schema: {e}")))?;
    Ok(client)
}

/// Get the cached client for `org_db_url`, connecting (and provisioning) lazily.
fn get_or_connect(org_db_url: &str) -> Result<Arc<Mutex<Client>>, AutterError> {
    {
        let map = CONNECTIONS.lock().expect("connection cache poisoned");
        if let Some(client) = map.get(org_db_url) {
            return Ok(client.clone());
        }
    }
    let client = Arc::new(Mutex::new(connect(org_db_url)?));
    let mut map = CONNECTIONS.lock().expect("connection cache poisoned");
    // Another thread may have connected while we were dialing — keep theirs.
    Ok(map.entry(org_db_url.to_string()).or_insert(client).clone())
}

/// Run a DB operation against the org's client.
///
/// Before using a cached connection we validate it with a cheap round-trip and,
/// if it has been dropped (idle timeout, server restart, network blip), discard
/// it and dial a fresh one. We can't rely on the operation itself surfacing the
/// failure: the notes/CAS closures count per-row errors internally rather than
/// propagating them, so a dead socket would otherwise look like an all-rows
/// failure instead of triggering a reconnect.
fn run<T>(
    org_db_url: &str,
    op: impl FnOnce(&mut Client) -> Result<T, postgres::Error>,
) -> Result<T, AutterError> {
    let arc = get_or_connect(org_db_url)?;
    {
        let mut guard = arc.lock().expect("org client mutex poisoned");
        if guard.is_valid(Duration::from_secs(5)).is_err() {
            // Cached connection is stale — drop it so the next get reconnects.
            drop(guard);
            CONNECTIONS
                .lock()
                .expect("connection cache poisoned")
                .remove(org_db_url);
            let fresh = get_or_connect(org_db_url)?;
            let mut guard = fresh.lock().expect("org client mutex poisoned");
            return op(&mut guard).map_err(map_db_err);
        }
        op(&mut guard).map_err(map_db_err)
    }
}

fn map_db_err(e: postgres::Error) -> AutterError {
    AutterError::Generic(format!("org database operation failed: {e}"))
}

/// Best-effort `data.push` audit row. Never returns an error (mirrors the old
/// backend, where audit failures were logged but never failed the request).
fn record_push(
    client: &mut Client,
    identity: &OrgIdentity,
    resource_id: Option<&str>,
    detail: &serde_json::Value,
) {
    let result = client.execute(
        "INSERT INTO cli_audit_log
            (id, event_type, actor_id, actor_email, token_id, resource_id, detail)
         VALUES (gen_random_uuid()::text, 'data.push', $1, $2, $3, $4, $5)",
        &[
            &identity.user_id,
            &identity.email,
            &identity.token_id,
            &resource_id,
            detail,
        ],
    );
    if let Err(e) = result {
        tracing::warn!("cli audit write failed: {e}");
    }
}

/// Upsert a batch of authorship notes, auditing each successful push.
pub fn upsert_notes(
    identity: &OrgIdentity,
    entries: &[NoteEntry],
    distinct_id: &str,
) -> Result<NotesUploadResponse, AutterError> {
    run(&identity.org_db_url, |client| {
        let mut success_count = 0usize;
        let mut failure_count = 0usize;

        for entry in entries {
            if entry.commit_sha.trim().is_empty() {
                failure_count += 1;
                continue;
            }

            let result = client.execute(
                "INSERT INTO authorship_notes (commit_sha, content, repo_url, uploaded_by, distinct_id)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (commit_sha)
                 DO UPDATE SET content = EXCLUDED.content,
                               repo_url = EXCLUDED.repo_url,
                               uploaded_by = EXCLUDED.uploaded_by,
                               distinct_id = EXCLUDED.distinct_id,
                               updated_at = now()",
                &[
                    &entry.commit_sha,
                    &entry.content,
                    &entry.repo_url,
                    &identity.user_id,
                    &distinct_id,
                ],
            );

            match result {
                Ok(_) => {
                    success_count += 1;
                    record_push(
                        client,
                        identity,
                        Some(&entry.commit_sha),
                        &serde_json::json!({ "kind": "notes", "distinct_id": distinct_id }),
                    );
                }
                Err(e) => {
                    tracing::warn!(commit = %entry.commit_sha, "note upsert failed: {e}");
                    failure_count += 1;
                }
            }
        }

        Ok(NotesUploadResponse {
            success_count,
            failure_count,
        })
    })
}

/// Read authorship notes by commit SHA (`commit_sha` → content).
pub fn read_notes(
    identity: &OrgIdentity,
    commit_shas: &[&str],
) -> Result<NotesReadResponse, AutterError> {
    if commit_shas.is_empty() {
        return Ok(NotesReadResponse {
            notes: HashMap::new(),
        });
    }
    let owned: Vec<String> = commit_shas.iter().map(|s| s.to_string()).collect();
    run(&identity.org_db_url, |client| {
        let rows = client.query(
            "SELECT commit_sha, content FROM authorship_notes WHERE commit_sha = ANY($1)",
            &[&owned],
        )?;
        let notes = rows
            .into_iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
            .collect::<HashMap<String, String>>();
        Ok(NotesReadResponse { notes })
    })
}

/// Store a batch of content-addressed prompt objects (dedup by hash).
pub fn upsert_cas(
    identity: &OrgIdentity,
    objects: &[CasObject],
    distinct_id: &str,
) -> Result<CasUploadResponse, AutterError> {
    run(&identity.org_db_url, |client| {
        let mut results = Vec::with_capacity(objects.len());
        let mut stored_hashes: Vec<String> = Vec::new();
        let mut success_count = 0usize;
        let mut failure_count = 0usize;

        for obj in objects {
            let metadata =
                serde_json::to_value(&obj.metadata).unwrap_or_else(|_| serde_json::json!({}));

            let result = client.execute(
                "INSERT INTO cas_objects (hash, content, metadata, uploaded_by)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (hash) DO NOTHING",
                &[&obj.hash, &obj.content, &metadata, &identity.user_id],
            );

            match result {
                Ok(_) => {
                    success_count += 1;
                    stored_hashes.push(obj.hash.clone());
                    results.push(CasUploadResult {
                        hash: obj.hash.clone(),
                        status: "ok".to_string(),
                        error: None,
                    });
                }
                Err(e) => {
                    tracing::warn!(hash = %obj.hash, "cas upsert failed: {e}");
                    failure_count += 1;
                    results.push(CasUploadResult {
                        hash: obj.hash.clone(),
                        status: "error".to_string(),
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        // One audit row per batch (per-object would be far too chatty).
        if !stored_hashes.is_empty() {
            record_push(
                client,
                identity,
                None,
                &serde_json::json!({
                    "kind": "cas",
                    "stored_count": stored_hashes.len(),
                    "failure_count": failure_count,
                    "hashes": stored_hashes,
                    "distinct_id": distinct_id,
                }),
            );
        }

        Ok(CasUploadResponse {
            results,
            success_count,
            failure_count,
        })
    })
}

/// Read CAS objects by hash, reporting a per-hash found/not-found status.
pub fn read_cas(
    identity: &OrgIdentity,
    hashes: &[&str],
) -> Result<CAPromptStoreReadResponse, AutterError> {
    if hashes.is_empty() {
        return Ok(CAPromptStoreReadResponse {
            results: Vec::new(),
            success_count: 0,
            failure_count: 0,
        });
    }
    let owned: Vec<String> = hashes.iter().map(|s| s.to_string()).collect();
    run(&identity.org_db_url, |client| {
        let rows = client.query(
            "SELECT hash, content FROM cas_objects WHERE hash = ANY($1)",
            &[&owned],
        )?;
        let found: HashMap<String, serde_json::Value> = rows
            .into_iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, serde_json::Value>(1)))
            .collect();

        let mut results = Vec::with_capacity(owned.len());
        let mut success_count = 0usize;
        let mut failure_count = 0usize;
        for hash in &owned {
            match found.get(hash) {
                Some(content) => {
                    success_count += 1;
                    results.push(CAPromptStoreReadResult {
                        hash: hash.clone(),
                        status: "ok".to_string(),
                        content: Some(content.clone()),
                        error: None,
                    });
                }
                None => {
                    failure_count += 1;
                    results.push(CAPromptStoreReadResult {
                        hash: hash.clone(),
                        status: "error".to_string(),
                        content: None,
                        error: Some("not found".to_string()),
                    });
                }
            }
        }

        Ok(CAPromptStoreReadResponse {
            results,
            success_count,
            failure_count,
        })
    })
}

/// Insert a batch of usage-metric events. Each event is stored as one row, with
/// its sparse `values`/`attrs` kept as JSONB. A content-hash `dedup_key` makes
/// the write idempotent, so re-flushing the local queue after a partial failure
/// can't create duplicates.
///
/// Returns the `(index, error)` pairs for any individual rows that failed; the
/// call itself only errors if the whole batch can't run (e.g. connection lost).
pub fn insert_metrics(
    identity: &OrgIdentity,
    events: &[MetricEvent],
    distinct_id: &str,
) -> Result<Vec<(usize, String)>, AutterError> {
    run(&identity.org_db_url, |client| {
        let mut errors = Vec::new();

        for (index, event) in events.iter().enumerate() {
            // Canonicalize so the dedup hash is stable regardless of map order.
            let canonical = serde_json_canonicalizer::to_string(event)
                .unwrap_or_else(|_| serde_json::to_string(event).unwrap_or_default());
            let mut hasher = Sha256::new();
            hasher.update(distinct_id.as_bytes());
            hasher.update([0u8]);
            hasher.update(canonical.as_bytes());
            let dedup_key = format!("{:x}", hasher.finalize());

            let values =
                serde_json::to_value(&event.values).unwrap_or_else(|_| serde_json::json!({}));
            let attrs =
                serde_json::to_value(&event.attrs).unwrap_or_else(|_| serde_json::json!({}));

            let result = client.execute(
                "INSERT INTO cli_metrics
                    (event_id, event_ts, event_values, event_attrs, uploaded_by, distinct_id, dedup_key)
                 VALUES ($1, to_timestamp($2), $3, $4, $5, $6, $7)
                 ON CONFLICT (dedup_key) DO NOTHING",
                &[
                    &(event.event_id as i32),
                    &(event.timestamp as f64),
                    &values,
                    &attrs,
                    &identity.user_id,
                    &distinct_id,
                    &dedup_key,
                ],
            );

            if let Err(e) = result {
                tracing::warn!(event_id = event.event_id, "metric insert failed: {e}");
                errors.push((index, e.to_string()));
            }
        }

        Ok(errors)
    })
}

/// Explicit per-commit authorship summary for cloud analytics.
#[derive(Debug, Clone)]
pub struct CommitAuthorshipSummaryRow {
    pub commit_sha: String,
    pub repo_url: Option<String>,
    pub branch: Option<String>,
    pub base_commit_sha: String,
    pub human_author: String,
    pub git_diff_added_lines: u64,
    pub git_diff_deleted_lines: u64,
    pub human_additions: u64,
    pub ai_additions: u64,
    pub ai_accepted: u64,
    pub unknown_additions: u64,
    pub human_percent: f64,
    pub ai_percent: f64,
    pub unknown_percent: f64,
    pub tool_model_breakdown: serde_json::Value,
    pub prompts: serde_json::Value,
    pub hunks: serde_json::Value,
}

/// Upsert a query-friendly commit authorship summary into the org database.
pub fn upsert_commit_authorship_summary(
    identity: &OrgIdentity,
    row: &CommitAuthorshipSummaryRow,
    distinct_id: &str,
) -> Result<(), AutterError> {
    run(&identity.org_db_url, |client| {
        client.execute(
            "INSERT INTO commit_authorship_summaries (
                commit_sha, repo_url, branch, base_commit_sha, human_author,
                git_diff_added_lines, git_diff_deleted_lines,
                human_additions, ai_additions, ai_accepted, unknown_additions,
                human_percent, ai_percent, unknown_percent,
                tool_model_breakdown, prompts, hunks, distinct_id, uploaded_by
            )
            VALUES (
                $1, $2, $3, $4, $5,
                $6, $7,
                $8, $9, $10, $11,
                $12, $13, $14,
                $15, $16, $17, $18, $19
            )
            ON CONFLICT (commit_sha)
            DO UPDATE SET
                repo_url = EXCLUDED.repo_url,
                branch = EXCLUDED.branch,
                base_commit_sha = EXCLUDED.base_commit_sha,
                human_author = EXCLUDED.human_author,
                git_diff_added_lines = EXCLUDED.git_diff_added_lines,
                git_diff_deleted_lines = EXCLUDED.git_diff_deleted_lines,
                human_additions = EXCLUDED.human_additions,
                ai_additions = EXCLUDED.ai_additions,
                ai_accepted = EXCLUDED.ai_accepted,
                unknown_additions = EXCLUDED.unknown_additions,
                human_percent = EXCLUDED.human_percent,
                ai_percent = EXCLUDED.ai_percent,
                unknown_percent = EXCLUDED.unknown_percent,
                tool_model_breakdown = EXCLUDED.tool_model_breakdown,
                prompts = EXCLUDED.prompts,
                hunks = EXCLUDED.hunks,
                distinct_id = EXCLUDED.distinct_id,
                uploaded_by = EXCLUDED.uploaded_by,
                updated_at = now()",
            &[
                &row.commit_sha,
                &row.repo_url,
                &row.branch,
                &row.base_commit_sha,
                &row.human_author,
                &(row.git_diff_added_lines as i64),
                &(row.git_diff_deleted_lines as i64),
                &(row.human_additions as i64),
                &(row.ai_additions as i64),
                &(row.ai_accepted as i64),
                &(row.unknown_additions as i64),
                &row.human_percent,
                &row.ai_percent,
                &row.unknown_percent,
                &row.tool_model_breakdown,
                &row.prompts,
                &row.hunks,
                &distinct_id,
                &identity.user_id,
            ],
        )?;

        record_push(
            client,
            identity,
            Some(&row.commit_sha),
            &serde_json::json!({
                "kind": "commit_authorship_summary",
                "distinct_id": distinct_id,
            }),
        );

        Ok(())
    })
}

/// Row for upserting aggregated file change counts.
#[derive(Debug, Clone)]
pub struct FileChangeCountRow {
    pub repo_url: String,
    pub file_path: String,
    pub change_count: u64,
    pub lines_added: u64,
    pub lines_deleted: u64,
    pub last_changed_at: u64,
}

/// Upsert aggregated file change counts into the org database.
///
/// Returns `(repo_url, file_path)` pairs for rows that failed individually.
pub fn upsert_file_change_counts(
    identity: &OrgIdentity,
    rows: &[FileChangeCountRow],
    distinct_id: &str,
) -> Result<Vec<(String, String)>, AutterError> {
    run(&identity.org_db_url, |client| {
        let mut failed = Vec::new();

        for row in rows {
            let result = client.execute(
                "INSERT INTO file_change_counts
                    (repo_url, file_path, change_count, lines_added, lines_deleted,
                     last_changed_at, distinct_id, uploaded_by)
                 VALUES ($1, $2, $3, $4, $5, to_timestamp($6), $7, $8)
                 ON CONFLICT (repo_url, file_path)
                 DO UPDATE SET
                    change_count = GREATEST(file_change_counts.change_count, EXCLUDED.change_count),
                    lines_added = GREATEST(file_change_counts.lines_added, EXCLUDED.lines_added),
                    lines_deleted = GREATEST(file_change_counts.lines_deleted, EXCLUDED.lines_deleted),
                    last_changed_at = GREATEST(file_change_counts.last_changed_at, EXCLUDED.last_changed_at),
                    distinct_id = EXCLUDED.distinct_id,
                    uploaded_by = EXCLUDED.uploaded_by,
                    updated_at = now()",
                &[
                    &row.repo_url,
                    &row.file_path,
                    &(row.change_count as i64),
                    &(row.lines_added as i64),
                    &(row.lines_deleted as i64),
                    &(row.last_changed_at as f64),
                    &distinct_id,
                    &identity.user_id,
                ],
            );

            if let Err(e) = result {
                tracing::warn!(
                    repo_url = %row.repo_url,
                    file_path = %row.file_path,
                    "file_change_counts upsert failed: {e}"
                );
                failed.push((row.repo_url.clone(), row.file_path.clone()));
            }
        }

        Ok(failed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a JWT-shaped string (header.payload.signature) with the given JSON
    /// payload, base64url-encoded without padding — just like a real token.
    fn fake_jwt(payload: serde_json::Value) -> String {
        let enc = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string().as_bytes())
        };
        format!(
            "{}.{}.{}",
            enc(&serde_json::json!({"alg": "RS256", "typ": "JWT"})),
            enc(&payload),
            "sig"
        )
    }

    #[test]
    fn decodes_org_routing_claims() {
        let token = fake_jwt(serde_json::json!({
            "sub": "user_123",
            "email": "dev@example.com",
            "org_db_url": "postgres://u:p@host/db",
            "token_id": "pat_abc",
        }));
        let id = identity_from_token(&token).unwrap();
        assert_eq!(id.org_db_url, "postgres://u:p@host/db");
        assert_eq!(id.user_id.as_deref(), Some("user_123"));
        assert_eq!(id.email.as_deref(), Some("dev@example.com"));
        assert_eq!(id.token_id.as_deref(), Some("pat_abc"));
    }

    #[test]
    fn missing_org_db_url_is_error() {
        let token = fake_jwt(serde_json::json!({ "sub": "user_123" }));
        assert!(identity_from_token(&token).is_err());
    }

    #[test]
    fn malformed_token_is_error() {
        assert!(identity_from_token("not-a-jwt").is_err());
    }

    #[test]
    fn optional_claims_default_to_none() {
        let token = fake_jwt(serde_json::json!({ "org_db_url": "postgres://x/y" }));
        let id = identity_from_token(&token).unwrap();
        assert!(id.user_id.is_none());
        assert!(id.email.is_none());
        assert!(id.token_id.is_none());
    }

    #[test]
    fn schema_provisions_repo_url_column() {
        // The org's authorship_notes table must carry repo_url so each note
        // records which repository it belongs to. The ALTER guarantees existing
        // org databases (created before this column) are migrated on connect.
        assert!(
            SCHEMA.contains("repo_url    TEXT"),
            "authorship_notes schema should declare a repo_url column"
        );
        assert!(
            SCHEMA.contains("ALTER TABLE authorship_notes ADD COLUMN IF NOT EXISTS repo_url TEXT"),
            "schema should backfill repo_url on pre-existing org databases"
        );
    }
}
