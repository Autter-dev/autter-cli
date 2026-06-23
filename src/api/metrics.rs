//! Metrics API endpoints

use crate::api::client::ApiClient;
use crate::api::org_db;
use crate::config;
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
    /// Write a metrics batch directly to the org's database.
    ///
    /// The destination database comes from the `org_db_url` claim in the
    /// context's access token (see [`crate::api::org_db`]); there is no
    /// intermediate backend.
    ///
    /// # Arguments
    /// * `batch` - The metrics batch to write
    ///
    /// # Returns
    /// * `Ok(MetricsUploadResponse)` - Response with per-event errors (empty = all success)
    /// * `Err(AutterError)` - When not authenticated or the batch can't run
    pub fn upload_metrics(
        &self,
        batch: &MetricsBatch,
    ) -> Result<MetricsUploadResponse, AutterError> {
        let identity = self.org_identity()?;
        let failed = org_db::insert_metrics(
            &identity,
            &batch.events,
            &config::get_or_create_distinct_id(),
        )?;
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
