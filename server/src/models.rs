//! Wire types for the storage endpoints.
//!
//! These mirror the structs the `autter` CLI serializes/deserializes in
//! `src/api/types.rs`. Keep them in sync — the field names and shapes are the
//! contract between the CLI and this server.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Error body returned to the CLI. The CLI reads the `error` field.
#[derive(Debug, Serialize)]
pub struct ApiErrorResponse {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

// ---- Notes ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NoteEntry {
    pub commit_sha: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct NotesUploadRequest {
    pub entries: Vec<NoteEntry>,
}

#[derive(Debug, Serialize)]
pub struct NotesUploadResponse {
    pub success_count: usize,
    pub failure_count: usize,
}

#[derive(Debug, Serialize)]
pub struct NotesReadResponse {
    pub notes: HashMap<String, String>,
}

// ---- CAS (prompt transcripts) ---------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CasObject {
    pub content: serde_json::Value,
    pub hash: String,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct CasUploadRequest {
    pub objects: Vec<CasObject>,
}

#[derive(Debug, Serialize)]
pub struct CasUploadResult {
    pub hash: String,
    pub status: String, // "ok" | "error"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CasUploadResponse {
    pub results: Vec<CasUploadResult>,
    pub success_count: usize,
    pub failure_count: usize,
}

/// Single result from a CAS batch read. Mirrors the CLI's
/// `CAPromptStoreReadResult`.
#[derive(Debug, Serialize)]
pub struct CasReadResult {
    pub hash: String,
    pub status: String, // "ok" | "error"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CasReadResponse {
    pub results: Vec<CasReadResult>,
    pub success_count: usize,
    pub failure_count: usize,
}
