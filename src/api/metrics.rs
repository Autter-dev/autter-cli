//! Metrics API endpoints

use crate::api::client::ApiClient;
use crate::error::AutterError;
use crate::metrics::MetricsBatch;
use crate::observability::log_error;
use serde::{Deserialize, Serialize};

/// Retry delay in seconds: single retry after 60s
const RETRY_DELAYS_SECS: [u64; 1] = [60];

/// Error for a single event in the batch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsUploadError {
    /// Index of the failed event in the request
    pub index: usize,
    /// Error message
    pub error: String,
}

/// Response from metrics upload endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsUploadResponse {
    /// List of errors (only failed events, empty = all success)
    pub errors: Vec<MetricsUploadError>,
}

impl MetricsUploadResponse {
    /// Get indices of successfully uploaded events
    #[allow(dead_code)]
    pub fn successful_indices(&self, batch_size: usize) -> Vec<usize> {
        let error_indices: std::collections::HashSet<_> =
            self.errors.iter().map(|e| e.index).collect();
        (0..batch_size)
            .filter(|i| !error_indices.contains(i))
            .collect()
    }
}

/// Upload metrics batch with retry logic.
///
/// Returns Ok(()) on success (200 response, even with partial errors).
/// Returns Err on failure after all retries exhausted.
///
/// Partial errors (200 + errors array) are logged to Sentry but not retried,
/// since validation errors won't succeed on retry.
pub fn upload_metrics_with_retry(
    client: &ApiClient,
    batch: &MetricsBatch,
    operation: &str,
) -> Result<(), AutterError> {
    // First attempt (no delay), then retry with delays
    for (attempt, delay_secs) in std::iter::once(&0u64)
        .chain(RETRY_DELAYS_SECS.iter())
        .enumerate()
    {
        if attempt > 0 {
            eprintln!(
                "[metrics] Retrying upload after {}s delay (attempt {}/{})",
                delay_secs,
                attempt + 1,
                RETRY_DELAYS_SECS.len() + 1
            );
            std::thread::sleep(std::time::Duration::from_secs(*delay_secs));
        }

        match client.upload_metrics(batch) {
            Ok(response) => {
                // 200 response - log any validation errors to Sentry
                for error in &response.errors {
                    log_error(
                        &AutterError::Generic(format!(
                            "Metrics {} error at index {}: {}",
                            operation, error.index, error.error
                        )),
                        Some(serde_json::json!({
                            "operation": operation,
                            "error_index": error.index
                        })),
                    );
                }
                if !response.errors.is_empty() {
                    crate::auth::notice::record_metrics_upload_stalled();
                    return Err(AutterError::Generic(format!(
                        "{} metric records were not accepted; upload is incomplete",
                        response.errors.len(),
                    )));
                }
                return Ok(());
            }
            Err(e) => {
                // Non-200 - will retry if attempts remain
                if attempt == RETRY_DELAYS_SECS.len() {
                    eprintln!("[metrics] All retries exhausted, giving up");
                    return Err(e);
                }
                eprintln!("[metrics] Upload failed: {}, will retry...", e);
            }
        }
    }

    Err(AutterError::Generic(
        "All upload retries exhausted".to_string(),
    ))
}

/// Metrics API endpoints
impl ApiClient {
    /// Write a metrics batch through the authenticated server-side data plane.
    ///
    /// # Arguments
    /// * `batch` - The metrics batch to write
    ///
    /// # Returns
    /// * `Ok(MetricsUploadResponse)` - Response with per-event errors (empty = all success)
    /// * `Err(AutterError)` - On network or server errors
    pub fn upload_metrics(
        &self,
        batch: &MetricsBatch,
    ) -> Result<MetricsUploadResponse, AutterError> {
        let response = self.context().post_json("/worker/metrics/upload", batch)?;
        let status_code = response.status_code;
        let body = response
            .as_str()
            .map_err(|e| AutterError::Generic(format!("Failed to read response body: {}", e)))?;
        if status_code != 200 {
            return Err(AutterError::Generic(format!(
                "Metrics upload failed with status {}: {}",
                status_code, body
            )));
        }
        let response: MetricsUploadResponse =
            serde_json::from_str(body).map_err(AutterError::JsonError)?;
        let failed = response
            .errors
            .iter()
            .map(|error| (error.index, error.error.clone()))
            .collect::<Vec<_>>();
        if failed.is_empty()
            && !batch.events.is_empty()
            && let Some(access_token) = self.context().auth_token.as_deref()
        {
            crate::auth::notice::record_metrics_upload(access_token);
        }
        let errors = failed
            .into_iter()
            .map(|(index, error)| MetricsUploadError { index, error })
            .collect();
        Ok(MetricsUploadResponse { errors })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_successful_indices() {
        let response = MetricsUploadResponse {
            errors: vec![
                MetricsUploadError {
                    index: 1,
                    error: "error".to_string(),
                },
                MetricsUploadError {
                    index: 3,
                    error: "error".to_string(),
                },
            ],
        };

        let successful = response.successful_indices(5);
        assert_eq!(successful, vec![0, 2, 4]);
    }

    #[test]
    fn test_successful_indices_empty_errors() {
        let response = MetricsUploadResponse { errors: vec![] };
        let successful = response.successful_indices(3);
        assert_eq!(successful, vec![0, 1, 2]);
    }

    #[test]
    fn test_successful_indices_all_errors() {
        let response = MetricsUploadResponse {
            errors: vec![
                MetricsUploadError {
                    index: 0,
                    error: "error".to_string(),
                },
                MetricsUploadError {
                    index: 1,
                    error: "error".to_string(),
                },
            ],
        };
        let successful = response.successful_indices(2);
        assert!(successful.is_empty());
    }
}
