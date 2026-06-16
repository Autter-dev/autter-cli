//! CAS (prompt-transcript) storage endpoints (written to the caller's org database).
//!
//! - `POST /worker/cas/upload` — store a batch of content-addressed objects.
//! - `GET  /worker/cas/?hashes=h1,h2` — read objects by hash.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;

use crate::error::AppError;
use crate::models::{
    CasReadResponse, CasReadResult, CasUploadRequest, CasUploadResponse, CasUploadResult,
};
use crate::state::AppState;

pub async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CasUploadRequest>,
) -> Result<Json<CasUploadResponse>, AppError> {
    let identity = state.verifier.authenticate(&headers).await?;
    let pool = state.pools.get(&identity.org_db_url).await?;

    let mut results = Vec::with_capacity(req.objects.len());
    let mut success_count = 0usize;
    let mut failure_count = 0usize;

    for obj in req.objects {
        let metadata =
            serde_json::to_value(&obj.metadata).unwrap_or_else(|_| serde_json::json!({}));

        // Dedup by hash: re-uploading the same content is a no-op.
        let result = sqlx::query(
            "INSERT INTO cas_objects (hash, content, metadata, uploaded_by)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (hash) DO NOTHING",
        )
        .bind(&obj.hash)
        .bind(&obj.content)
        .bind(&metadata)
        .bind(&identity.user_id)
        .execute(&pool)
        .await;

        match result {
            Ok(_) => {
                success_count += 1;
                results.push(CasUploadResult {
                    hash: obj.hash,
                    status: "ok".to_string(),
                    error: None,
                });
            }
            Err(e) => {
                tracing::warn!(hash = %obj.hash, "cas upsert failed: {e}");
                failure_count += 1;
                results.push(CasUploadResult {
                    hash: obj.hash,
                    status: "error".to_string(),
                    error: Some(e.to_string()),
                });
            }
        }
    }

    Ok(Json(CasUploadResponse {
        results,
        success_count,
        failure_count,
    }))
}

#[derive(Debug, Deserialize)]
pub struct HashesQuery {
    hashes: Option<String>,
}

pub async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashesQuery>,
) -> Result<Json<CasReadResponse>, AppError> {
    let identity = state.verifier.authenticate(&headers).await?;

    let hashes: Vec<String> = q
        .hashes
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    if hashes.is_empty() {
        return Ok(Json(CasReadResponse {
            results: Vec::new(),
            success_count: 0,
            failure_count: 0,
        }));
    }

    let pool = state.pools.get(&identity.org_db_url).await?;

    let rows = sqlx::query_as::<_, (String, serde_json::Value)>(
        "SELECT hash, content FROM cas_objects WHERE hash = ANY($1)",
    )
    .bind(&hashes)
    .fetch_all(&pool)
    .await?;

    let found: HashMap<String, serde_json::Value> = rows.into_iter().collect();

    let mut results = Vec::with_capacity(hashes.len());
    let mut success_count = 0usize;
    let mut failure_count = 0usize;

    for hash in hashes {
        match found.get(&hash) {
            Some(content) => {
                success_count += 1;
                results.push(CasReadResult {
                    hash,
                    status: "ok".to_string(),
                    content: Some(content.clone()),
                    error: None,
                });
            }
            None => {
                failure_count += 1;
                results.push(CasReadResult {
                    hash,
                    status: "error".to_string(),
                    content: None,
                    error: Some("not found".to_string()),
                });
            }
        }
    }

    Ok(Json(CasReadResponse {
        results,
        success_count,
        failure_count,
    }))
}
