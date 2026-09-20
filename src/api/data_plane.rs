//! Authenticated API-fronted writes for data that belongs to an organization.
//!
//! The CLI deliberately has no database-routing or database-connection logic.
//! Organization resolution and all database access happen behind these HTTPS
//! endpoints on the server.

use crate::api::client::ApiClient;
use crate::api::types::{CommitAuthorshipSummary, FileChangeCount, FileChangeUploadResponse};
use crate::error::AutterError;

fn require_success(
    response: crate::http::Response,
    operation: &str,
) -> Result<String, AutterError> {
    let status_code = response.status_code;
    let body = response
        .as_str()
        .map_err(|e| AutterError::Generic(format!("Failed to read response body: {}", e)))?;
    if status_code != 200 {
        return Err(AutterError::Generic(format!(
            "{} failed with status {}: {}",
            operation, status_code, body
        )));
    }
    Ok(body.to_string())
}

impl ApiClient {
    pub fn upload_commit_authorship_summary(
        &self,
        row: &CommitAuthorshipSummary,
        distinct_id: &str,
    ) -> Result<(), AutterError> {
        let request = serde_json::json!({
            "summary": row,
            "distinct_id": distinct_id,
        });
        let body = require_success(
            self.context()
                .post_json("/worker/commit-authorship/upload", &request)?,
            "Commit authorship upload",
        )?;
        let _ = body;
        Ok(())
    }

    pub fn upload_file_change_counts(
        &self,
        rows: &[FileChangeCount],
        distinct_id: &str,
    ) -> Result<FileChangeUploadResponse, AutterError> {
        let request = serde_json::json!({
            "rows": rows,
            "distinct_id": distinct_id,
        });
        let body = require_success(
            self.context()
                .post_json("/worker/file-changes/upload", &request)?,
            "File-change upload",
        )?;
        serde_json::from_str(&body).map_err(AutterError::JsonError)
    }

    /// Probe the server-side data plane without exposing or opening an org DB
    /// connection from the CLI.
    pub fn check_data_plane(&self) -> Result<(), AutterError> {
        let response = self.context().get("/worker/health")?;
        let _ = require_success(response, "Data-plane health check")?;
        Ok(())
    }
}
