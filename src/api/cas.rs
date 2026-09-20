//! CAS (prompt-transcript) storage through the authenticated Autter API.

use crate::api::client::ApiClient;
use crate::api::types::{CAPromptStoreReadResponse, CasUploadRequest, CasUploadResponse};
use crate::error::AutterError;

/// CAS API endpoints
impl ApiClient {
    /// Store CAS objects in the server-side org database (dedup by hash).
    ///
    /// # Arguments
    /// * `request` - The CAS upload request containing objects to upload
    ///
    /// # Returns
    /// * `Ok(CasUploadResponse)` - Per-object results plus counts
    /// * `Err(AutterError)` - On network or server errors
    pub fn upload_cas(&self, request: CasUploadRequest) -> Result<CasUploadResponse, AutterError> {
        let response = self.context().post_json("/worker/cas/upload", &request)?;
        let status_code = response.status_code;
        let body = response
            .as_str()
            .map_err(|e| AutterError::Generic(format!("Failed to read response body: {}", e)))?;
        if status_code == 200 {
            serde_json::from_str(body).map_err(AutterError::JsonError)
        } else {
            Err(AutterError::Generic(format!(
                "CAS upload failed with status {}: {}",
                status_code, body
            )))
        }
    }

    /// Read CAS objects by hash from the org's database.
    ///
    /// # Arguments
    /// * `hashes` - Slice of CAS hashes to fetch
    ///
    /// # Returns
    /// * `Ok(CAPromptStoreReadResponse)` - Response with results for each hash
    /// * `Err(AutterError)` - On invalid input, auth, or DB errors
    pub fn read_ca_prompt_store(
        &self,
        hashes: &[&str],
    ) -> Result<CAPromptStoreReadResponse, AutterError> {
        // Validate all hashes are hex-only to guard against malformed input.
        for hash in hashes {
            if !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(AutterError::Generic(format!(
                    "CAS hash contains non-hex characters: {}",
                    hash
                )));
            }
        }

        let endpoint = format!("/worker/cas/?hashes={}", hashes.join(","));
        let response = self.context().get(&endpoint)?;
        let status_code = response.status_code;
        let body = response
            .as_str()
            .map_err(|e| AutterError::Generic(format!("Failed to read response body: {}", e)))?;
        if status_code == 200 {
            serde_json::from_str(body).map_err(AutterError::JsonError)
        } else {
            Err(AutterError::Generic(format!(
                "CAS read failed with status {}: {}",
                status_code, body
            )))
        }
    }
}
