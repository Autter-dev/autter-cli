//! Authorship-note storage endpoints.
//!
//! - `POST /worker/notes/upload` — upsert a batch of notes for the caller's org.
//! - `GET  /worker/notes/?commits=sha1,sha2` — read notes by commit SHA.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth::authenticate;
use crate::error::AppError;
use crate::models::{NotesReadResponse, NotesUploadRequest, NotesUploadResponse};
use crate::state::AppState;

pub async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NotesUploadRequest>,
) -> Result<Json<NotesUploadResponse>, AppError> {
    let auth = authenticate(&state, &headers).await?;

    let mut success_count = 0usize;
    let mut failure_count = 0usize;

    for entry in req.entries {
        if entry.commit_sha.trim().is_empty() {
            failure_count += 1;
            continue;
        }

        let result = sqlx::query(
            "INSERT INTO authorship_notes (org_id, commit_sha, content, uploaded_by, distinct_id)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (org_id, commit_sha)
             DO UPDATE SET content = EXCLUDED.content, updated_at = now()",
        )
        .bind(auth.org_id)
        .bind(&entry.commit_sha)
        .bind(&entry.content)
        .bind(auth.user_id)
        .bind(&auth.distinct_id)
        .execute(&state.pool)
        .await;

        match result {
            Ok(_) => success_count += 1,
            Err(e) => {
                tracing::warn!(commit = %entry.commit_sha, "note upsert failed: {e}");
                failure_count += 1;
            }
        }
    }

    Ok(Json(NotesUploadResponse {
        success_count,
        failure_count,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CommitsQuery {
    commits: Option<String>,
}

pub async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CommitsQuery>,
) -> Result<Json<NotesReadResponse>, AppError> {
    let auth = authenticate(&state, &headers).await?;

    let commits: Vec<String> = q
        .commits
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    if commits.is_empty() {
        return Ok(Json(NotesReadResponse {
            notes: HashMap::new(),
        }));
    }

    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT commit_sha, content FROM authorship_notes
         WHERE org_id = $1 AND commit_sha = ANY($2)",
    )
    .bind(auth.org_id)
    .bind(&commits)
    .fetch_all(&state.pool)
    .await?;

    let notes = rows.into_iter().collect::<HashMap<String, String>>();
    Ok(Json(NotesReadResponse { notes }))
}
