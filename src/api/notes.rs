//! Authorship-note storage, written directly to the org's own Postgres.
//!
//! The connection URL comes from the `org_db_url` claim in the context's access
//! token (see [`crate::api::org_db`]); there is no intermediate backend. Callers
//! should still gate on `is_logged_in()` / `has_api_key()` so we only attempt a
//! write when the user is authenticated (matching the CAS pattern).

use crate::api::client::ApiClient;
use crate::api::org_db;
use crate::api::types::{NotesReadResponse, NotesUploadRequest, NotesUploadResponse};
use crate::config;
use crate::error::AutterError;

impl ApiClient {
    /// Upload a batch of authorship notes to the org's database.
    ///
    /// # Arguments
    /// * `request` - The notes upload request containing entries to upload
    ///
    /// # Returns
    /// * `Ok(NotesUploadResponse)` - Success response with counts
    /// * `Err(AutterError)` - When not authenticated or the DB write fails
    pub fn upload_notes(
        &self,
        request: NotesUploadRequest,
    ) -> Result<NotesUploadResponse, AutterError> {
        let identity = self.org_identity()?;
        org_db::upsert_notes(
            &identity,
            &request.entries,
            &config::get_or_create_distinct_id(),
        )
    }

    /// Read authorship notes by commit SHAs.
    ///
    /// Returns an empty map for any SHAs not found.
    ///
    /// # Arguments
    /// * `commit_shas` - Slice of hex commit SHAs to fetch
    ///
    /// # Returns
    /// * `Ok(NotesReadResponse)` - Response mapping commit_sha → note content
    /// * `Err(AutterError)` - On invalid input, auth, or DB errors
    pub fn read_notes(&self, commit_shas: &[&str]) -> Result<NotesReadResponse, AutterError> {
        // Validate that all SHAs are hex strings before querying.
        for sha in commit_shas {
            if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(AutterError::Generic(format!(
                    "Commit SHA contains non-hex characters: {}",
                    sha
                )));
            }
        }

        let identity = self.org_identity()?;
        org_db::read_notes(&identity, commit_shas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::{ApiClient, ApiContext};
    use crate::api::types::NoteEntry;

    #[test]
    fn test_read_notes_rejects_non_hex_sha() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        let client = ApiClient::new(ctx);

        let result = client.read_notes(&["not-a-hex-sha"]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("non-hex"),
            "error should mention non-hex: {}",
            err
        );
    }

    #[test]
    fn test_read_notes_accepts_valid_hex_sha() {
        // A valid hex SHA should pass validation (the actual HTTP call will fail
        // because there is no real server, but we are testing input validation only).
        let ctx = ApiContext::without_auth(Some("https://127.0.0.1:1".to_string()));
        let client = ApiClient::new(ctx);

        let valid_sha = "abc123def456abc123def456abc123def456abc1";
        // This will fail on the HTTP call, not on validation
        let result = client.read_notes(&[valid_sha]);
        // The error should be network-related, not a validation error
        if let Err(e) = result {
            let msg = e.to_string();
            assert!(
                !msg.contains("non-hex"),
                "should not fail hex validation for valid SHA, got: {}",
                msg
            );
        }
    }

    #[test]
    fn test_notes_upload_request_serialization() {
        let request = NotesUploadRequest {
            entries: vec![NoteEntry {
                commit_sha: "abc123".to_string(),
                content: "authorship data".to_string(),
                repo_url: Some("https://github.com/acme/repo.git".to_string()),
            }],
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("abc123"));
        assert!(json.contains("authorship data"));
        assert!(json.contains("entries"));
        // The repository the note belongs to must travel in the payload so the
        // org database can store which repo each note came from.
        assert!(json.contains("repo_url"));
        assert!(json.contains("https://github.com/acme/repo.git"));
    }

    #[test]
    fn test_note_entry_repo_url_round_trips() {
        // A note carrying a repo_url survives a serialize → deserialize cycle,
        // matching what the daemon flush sends and what the org DB upserts.
        let entry = NoteEntry {
            commit_sha: "deadbeef".to_string(),
            content: "note body".to_string(),
            repo_url: Some("git@github.com:acme/widgets.git".to_string()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: NoteEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, entry);
        assert_eq!(
            decoded.repo_url.as_deref(),
            Some("git@github.com:acme/widgets.git")
        );
    }

    #[test]
    fn test_note_entry_without_repo_url_omits_field() {
        // When the repo couldn't be determined, repo_url is None and is omitted
        // from the wire payload (skip_serializing_if), decoding back to None.
        let entry = NoteEntry {
            commit_sha: "cafef00d".to_string(),
            content: "note body".to_string(),
            repo_url: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("repo_url"));
        let decoded: NoteEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.repo_url, None);
    }

    #[test]
    fn test_notes_read_response_deserialization() {
        let json = r#"{"notes": {"abc123": "content1", "def456": "content2"}}"#;
        let response: NotesReadResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.notes.get("abc123"), Some(&"content1".to_string()));
        assert_eq!(response.notes.get("def456"), Some(&"content2".to_string()));
    }

    #[test]
    fn test_notes_upload_response_deserialization() {
        let json = r#"{"success_count": 5, "failure_count": 1}"#;
        let response: NotesUploadResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.success_count, 5);
        assert_eq!(response.failure_count, 1);
    }
}
