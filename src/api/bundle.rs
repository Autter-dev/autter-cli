use crate::api::client::ApiClient;
use crate::api::types::{ApiErrorResponse, CreateBundleRequest, CreateBundleResponse};
use crate::error::AutterError;

/// Bundle API endpoints
impl ApiClient {
    /// Create a new bundle by posting to /api/bundle
    ///
    /// # Arguments
    /// * `request` - The bundle creation request
    ///
    /// # Returns
    /// * `Ok(CreateBundleResponse)` - Success response with bundle ID and URL
    /// * `Err(AutterError)` - Error response
    ///
    /// # Errors
    /// * Returns `AutterError::Generic` for HTTP errors
    /// * Returns `AutterError::JsonError` for JSON parsing errors
    /// * Returns `AutterError::Generic` with error details for API errors (400, 500, etc.)
    pub fn create_bundle(
        &self,
        request: CreateBundleRequest,
    ) -> Result<CreateBundleResponse, AutterError> {
        let response = self.context().post_json("/api/bundles", &request)?;
        let status_code = response.status_code;

        let body = response
            .as_str()
            .map_err(|e| AutterError::Generic(format!("Failed to read response body: {}", e)))?;

        match status_code {
            200 => {
                let bundle_response: CreateBundleResponse =
                    serde_json::from_str(body).map_err(AutterError::JsonError)?;
                Ok(bundle_response)
            }
            400 => {
                // Try to parse error response
                let error_response: ApiErrorResponse =
                    serde_json::from_str(body).unwrap_or_else(|_| ApiErrorResponse {
                        error: "Invalid request body".to_string(),
                        details: Some(serde_json::Value::String(body.to_string())),
                    });
                Err(AutterError::Generic(format!(
                    "Bad Request: {}",
                    error_response.error
                )))
            }
            500 => {
                let error_response: ApiErrorResponse =
                    serde_json::from_str(body).unwrap_or_else(|_| ApiErrorResponse {
                        error: "Internal server error".to_string(),
                        details: None,
                    });
                Err(AutterError::Generic(format!(
                    "Internal Server Error: {}",
                    error_response.error
                )))
            }
            _ => Err(AutterError::Generic(format!(
                "Unexpected status code {}: {}",
                status_code, body
            ))),
        }
    }
}
