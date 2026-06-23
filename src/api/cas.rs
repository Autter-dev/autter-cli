//! CAS (prompt-transcript) storage, written directly to the org's own Postgres.
//!
//! The connection URL comes from the `org_db_url` claim in the context's access
//! token (see [`crate::api::org_db`]); there is no intermediate backend.

use crate::api::client::ApiClient;
use crate::api::org_db;
use crate::api::types::{CAPromptStoreReadResponse, CasUploadRequest, CasUploadResponse};
use crate::config;
use crate::error::AutterError;

/// CAS API endpoints
impl ApiClient {
    /// Store CAS objects in the org's database (dedup by hash).
    ///
    /// # Arguments
    /// * `request` - The CAS upload request containing objects to upload
    ///
    /// # Returns
    /// * `Ok(CasUploadResponse)` - Per-object results plus counts
    /// * `Err(AutterError)` - When not authenticated or the DB write fails
    pub fn upload_cas(&self, request: CasUploadRequest) -> Result<CasUploadResponse, AutterError> {
        let identity = self.org_identity()?;
        org_db::upsert_cas(
            &identity,
            &request.objects,
            &config::get_or_create_distinct_id(),
        )
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

        let identity = self.org_identity()?;
        org_db::read_cas(&identity, hashes)
    }
}
