//! Authorship-note storage through the authenticated Autter API.

use crate::api::client::ApiClient;
use crate::api::types::{
    ApiErrorResponse, NoteRecordsResponse, NotesReadResponse, NotesUploadRequest,
    NotesUploadResponse,
};
use crate::error::AutterError;

impl ApiClient {
    /// Upload a batch of authorship notes to the server-side org database.
    ///
    /// # Arguments
    /// * `request` - The notes upload request containing entries to upload
    ///
    /// # Returns
    /// * `Ok(NotesUploadResponse)` - Success response with counts
    /// * `Err(AutterError)` - On network or server errors
    pub fn upload_notes(
        &self,
        request: NotesUploadRequest,
    ) -> Result<NotesUploadResponse, AutterError> {
        let response = self.context().post_json("/worker/notes/upload", &request)?;
        let status_code = response.status_code;
        let body = response
            .as_str()
            .map_err(|e| AutterError::Generic(format!("Failed to read response body: {}", e)))?;

        match status_code {
            200 => serde_json::from_str(body).map_err(AutterError::JsonError),
            400 => {
                let error: ApiErrorResponse =
                    serde_json::from_str(body).unwrap_or(ApiErrorResponse {
                        error: "Invalid request body".to_string(),
                        details: Some(serde_json::Value::String(body.to_string())),
                    });
                Err(AutterError::Generic(format!(
                    "Bad Request: {}",
                    error.error
                )))
            }
            _ => Err(AutterError::Generic(format!(
                "Notes upload failed with status {}: {}",
                status_code, body
            ))),
        }
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
        validate_commit_shas(commit_shas)?;

        let endpoint = format!("/worker/notes/?commits={}", commit_shas.join(","));
        let response = self.context().get(&endpoint)?;
        let status_code = response.status_code;
        let body = response
            .as_str()
            .map_err(|e| AutterError::Generic(format!("Failed to read response body: {}", e)))?;

        match status_code {
            200 => serde_json::from_str(body).map_err(AutterError::JsonError),
            404 => Ok(NotesReadResponse {
                notes: std::collections::HashMap::new(),
            }),
            _ => Err(AutterError::Generic(format!(
                "Notes read failed with status {}: {}",
                status_code, body
            ))),
        }
    }

    /// Read the server's record of every version of these commits' notes
    /// (`GET /worker/notes/record`, at most [`MAX_RECORD_COMMITS`] per call).
    ///
    /// Returns `Ok(None)` when the server has no such endpoint (404: a server
    /// older than the record), so callers can say the record was not checked
    /// rather than fail. Commits the server has no record for are absent from
    /// the response.
    pub fn read_note_records(
        &self,
        commit_shas: &[&str],
    ) -> Result<Option<NoteRecordsResponse>, AutterError> {
        validate_commit_shas(commit_shas)?;
        if commit_shas.len() > MAX_RECORD_COMMITS {
            return Err(AutterError::Generic(format!(
                "At most {MAX_RECORD_COMMITS} commits per note record request, got {}",
                commit_shas.len()
            )));
        }
        if commit_shas.is_empty() {
            return Ok(Some(NoteRecordsResponse::default()));
        }

        let endpoint = format!("/worker/notes/record?commits={}", commit_shas.join(","));
        let response = self.context().get(&endpoint)?;
        let status_code = response.status_code;
        let body = response
            .as_str()
            .map_err(|e| AutterError::Generic(format!("Failed to read response body: {}", e)))?;

        match status_code {
            200 => serde_json::from_str(body)
                .map(Some)
                .map_err(AutterError::JsonError),
            404 => Ok(None),
            401 | 403 => Err(AutterError::Generic(format!(
                "the server refused the request (status {status_code}); run `autter login`"
            ))),
            _ => Err(AutterError::Generic(format!(
                "note record read failed with status {status_code}"
            ))),
        }
    }
}

/// The most commits one `GET /worker/notes/record` request may name.
pub const MAX_RECORD_COMMITS: usize = 100;

/// Commit SHAs go into a query string: only hex digits are allowed.
fn validate_commit_shas(commit_shas: &[&str]) -> Result<(), AutterError> {
    for sha in commit_shas {
        if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(AutterError::Generic(format!(
                "Commit SHA contains non-hex characters: {}",
                sha
            )));
        }
    }
    Ok(())
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
    fn test_read_note_records_rejects_non_hex_sha() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        let client = ApiClient::new(ctx);
        let err = client
            .read_note_records(&["abc123", "abc&commits=x"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-hex"), "{err}");
    }

    #[test]
    fn test_read_note_records_rejects_more_than_a_chunk() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        let client = ApiClient::new(ctx);
        let shas: Vec<String> = (0..=MAX_RECORD_COMMITS)
            .map(|i| format!("{i:040x}"))
            .collect();
        let refs: Vec<&str> = shas.iter().map(String::as_str).collect();
        let err = client.read_note_records(&refs).unwrap_err().to_string();
        assert!(err.contains("At most 100"), "{err}");
    }

    #[test]
    fn test_read_note_records_empty_input_makes_no_request() {
        // An unreachable server: an empty request must not contact it.
        let ctx = ApiContext::without_auth(Some("http://127.0.0.1:1".to_string()));
        let client = ApiClient::new(ctx);
        let response = client.read_note_records(&[]).unwrap().unwrap();
        assert!(response.records.is_empty());
    }

    #[test]
    fn test_note_records_response_deserialization() {
        let json = r#"{"records": {
            "abc123": {"versions": [
                {"content_sha256": "aa", "integrity_digest": "dd", "uploaded_by": "user_1",
                 "recorded_at": "2026-11-01T10:00:00Z", "seeded": true},
                {"content_sha256": "bb", "integrity_digest": null, "uploaded_by": null,
                 "recorded_at": "2026-11-02T10:00:00Z", "seeded": false}
            ]},
            "def456": {"versions": []}
        }}"#;
        let response: NoteRecordsResponse = serde_json::from_str(json).unwrap();
        let versions = &response.records["abc123"].versions;
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].content_sha256, "aa");
        assert_eq!(versions[0].integrity_digest.as_deref(), Some("dd"));
        assert_eq!(versions[0].uploaded_by.as_deref(), Some("user_1"));
        assert!(versions[0].seeded);
        assert_eq!(versions[1].integrity_digest, None);
        assert_eq!(versions[1].uploaded_by, None);
        assert_eq!(versions[1].recorded_at, "2026-11-02T10:00:00Z");
        assert!(!versions[1].seeded);
        assert!(response.records["def456"].versions.is_empty());
    }

    #[test]
    fn test_note_records_response_tolerates_missing_optional_fields() {
        // Fields a server may leave out (older or trimmed responses).
        let json = r#"{"records": {"abc": {"versions": [{"content_sha256": "aa"}]}}}"#;
        let response: NoteRecordsResponse = serde_json::from_str(json).unwrap();
        let v = &response.records["abc"].versions[0];
        assert_eq!(v.integrity_digest, None);
        assert_eq!(v.uploaded_by, None);
        assert!(!v.seeded);
        let empty: NoteRecordsResponse = serde_json::from_str("{}").unwrap();
        assert!(empty.records.is_empty());
    }

    #[test]
    fn test_notes_upload_response_deserialization() {
        let json = r#"{"success_count": 5, "failure_count": 1}"#;
        let response: NotesUploadResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.success_count, 5);
        assert_eq!(response.failure_count, 1);
    }
}
