//! Metrics API endpoints

use crate::api::client::ApiClient;
use crate::error::AutterError;
use crate::metrics::MetricsBatch;
use crate::observability::log_error;
use serde::{Deserialize, Serialize};

/// Retry delay in seconds: single retry after 60s
const RETRY_DELAYS_SECS: [u64; 1] = [60];

/// The endpoint usage metrics are written to. Named in `autter doctor` and
/// `autter sync status` when uploads fail.
pub const METRICS_UPLOAD_ENDPOINT: &str = "/worker/metrics/upload";

/// Why a metrics upload got no usable answer. Each kind says whether retrying
/// can help and whether a smaller batch might.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricsUploadFailure {
    /// No response within the client timeout. The server may still be
    /// working, or a proxy may have cut a long request; a smaller batch
    /// finishes sooner.
    Timeout { after_secs: Option<u64> },
    /// The server answered with a non-200 status.
    Http { status: u16, body: String },
    /// No response for another reason (DNS, refused connection, TLS, reset).
    Network { message: String },
    /// A 200 whose body was not the expected JSON.
    InvalidResponse { message: String },
    /// A 200 that accepted none of the events, for a reason a later retry
    /// may fix (validation verdicts are dropped, not reported this way).
    NotAccepted { count: usize, error: String },
}

/// What a failure kind means for the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricsFailureKind {
    Timeout,
    /// HTTP 413, 504 and other "too big or too slow" answers.
    TooLarge,
    /// HTTP 401/403: the login is not accepted. Retrying waits for a new login.
    Auth,
    /// HTTP 4xx other than the above: the request itself is wrong.
    Rejected,
    /// HTTP 5xx (other than 504): the server failed.
    Server,
    Network,
    InvalidResponse,
}

impl MetricsUploadFailure {
    pub fn kind(&self) -> MetricsFailureKind {
        match self {
            Self::Timeout { .. } => MetricsFailureKind::Timeout,
            Self::Http { status, .. } => match *status {
                413 | 504 => MetricsFailureKind::TooLarge,
                401 | 403 => MetricsFailureKind::Auth,
                400..=499 => MetricsFailureKind::Rejected,
                _ => MetricsFailureKind::Server,
            },
            Self::Network { .. } => MetricsFailureKind::Network,
            Self::InvalidResponse { .. } => MetricsFailureKind::InvalidResponse,
            Self::NotAccepted { .. } => MetricsFailureKind::Rejected,
        }
    }

    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// A smaller batch could succeed where this one failed.
    pub fn should_split(&self) -> bool {
        matches!(
            self.kind(),
            MetricsFailureKind::Timeout | MetricsFailureKind::TooLarge
        )
    }

    /// Short description for `autter doctor` and `autter sync status`, e.g.
    /// "timeout after 30s" or "HTTP 413: request entity too large".
    pub fn summary(&self) -> String {
        match self {
            Self::Timeout {
                after_secs: Some(secs),
            } => format!("timeout after {secs}s (no response)"),
            Self::Timeout { after_secs: None } => "timeout (no response)".to_string(),
            Self::Http { status, body } => {
                let body: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
                let body: String = body.chars().take(160).collect();
                if body.is_empty() {
                    format!("HTTP {status}")
                } else {
                    format!("HTTP {status}: {body}")
                }
            }
            Self::Network { message } => format!("network error: {message}"),
            Self::InvalidResponse { message } => format!("unreadable response: {message}"),
            Self::NotAccepted { count, error } if error.is_empty() => {
                format!("server accepted none of {count} events")
            }
            Self::NotAccepted { count, error } => {
                format!("server accepted none of {count} events: {error}")
            }
        }
    }
}

impl std::fmt::Display for MetricsUploadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "POST {METRICS_UPLOAD_ENDPOINT} failed: {}",
            self.summary()
        )
    }
}

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
        self.upload_metrics_classified(batch)
            .map_err(|failure| AutterError::Generic(failure.to_string()))
    }

    /// [`Self::upload_metrics`], with the failure classified (timeout, HTTP
    /// status, network, unreadable answer) so the caller can decide whether to
    /// retry, retry smaller, or wait.
    pub fn upload_metrics_classified(
        &self,
        batch: &MetricsBatch,
    ) -> Result<MetricsUploadResponse, MetricsUploadFailure> {
        let response = match self
            .context()
            .post_json_classified(METRICS_UPLOAD_ENDPOINT, batch)
        {
            Ok(Ok(response)) => response,
            Ok(Err(transport)) if transport.timed_out => {
                return Err(MetricsUploadFailure::Timeout {
                    after_secs: self.context().timeout_secs,
                });
            }
            Ok(Err(transport)) => {
                return Err(MetricsUploadFailure::Network {
                    message: transport.message,
                });
            }
            Err(error) => {
                return Err(MetricsUploadFailure::Network {
                    message: error.to_string(),
                });
            }
        };
        let status_code = response.status_code;
        let body = String::from_utf8_lossy(response.as_bytes()).into_owned();
        if status_code != 200 {
            return Err(MetricsUploadFailure::Http {
                status: status_code,
                body,
            });
        }
        let response: MetricsUploadResponse =
            serde_json::from_str(&body).map_err(|error| MetricsUploadFailure::InvalidResponse {
                message: error.to_string(),
            })?;
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

    fn http(status: u16) -> MetricsUploadFailure {
        MetricsUploadFailure::Http {
            status,
            body: String::new(),
        }
    }

    #[test]
    fn failures_are_classified_by_what_a_retry_can_fix() {
        use MetricsFailureKind::*;
        for (failure, kind, split) in [
            (
                MetricsUploadFailure::Timeout {
                    after_secs: Some(30),
                },
                Timeout,
                true,
            ),
            (http(413), TooLarge, true),
            (http(504), TooLarge, true),
            (http(401), Auth, false),
            (http(403), Auth, false),
            (http(400), Rejected, false),
            (http(500), Server, false),
            (http(502), Server, false),
            (
                MetricsUploadFailure::Network {
                    message: "connection refused".to_string(),
                },
                Network,
                false,
            ),
            (
                MetricsUploadFailure::InvalidResponse {
                    message: "expected value".to_string(),
                },
                InvalidResponse,
                false,
            ),
        ] {
            assert_eq!(failure.kind(), kind, "{failure:?}");
            assert_eq!(failure.should_split(), split, "{failure:?}");
        }
    }

    #[test]
    fn failure_text_names_the_endpoint_and_the_error() {
        let timeout = MetricsUploadFailure::Timeout {
            after_secs: Some(30),
        };
        assert_eq!(
            timeout.to_string(),
            "POST /worker/metrics/upload failed: timeout after 30s (no response)"
        );
        let too_large = MetricsUploadFailure::Http {
            status: 413,
            body: "{\"error\":\n  \"too many events\"}".to_string(),
        };
        assert_eq!(
            too_large.summary(),
            "HTTP 413: {\"error\": \"too many events\"}"
        );
        assert_eq!(http(502).summary(), "HTTP 502");
        // A huge HTML error page is cut short.
        let html = MetricsUploadFailure::Http {
            status: 502,
            body: "<html>".repeat(200),
        };
        assert!(html.summary().len() < 200);
    }

    /// A socket that accepts the request and never answers, then (when
    /// `status` is set) one that answers with that status.
    fn stub(status: Option<u16>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(300)));
                let mut buffer = [0u8; 4096];
                while matches!(stream.read(&mut buffer), Ok(read) if read > 0) {}
                match status {
                    Some(status) => {
                        let body = "{\"error\":\"too large\"}";
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 {status} Nope\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        );
                    }
                    None => std::thread::sleep(std::time::Duration::from_secs(4)),
                }
            }
        });
        base_url
    }

    fn client(base_url: String) -> ApiClient {
        ApiClient::new(crate::api::ApiContext::without_auth(Some(base_url)).with_timeout(1))
    }

    #[test]
    fn a_server_that_never_answers_is_a_timeout() {
        let failure = client(stub(None))
            .upload_metrics_classified(&MetricsBatch::new(Vec::new()))
            .unwrap_err();
        assert_eq!(
            failure,
            MetricsUploadFailure::Timeout {
                after_secs: Some(1)
            }
        );
    }

    #[test]
    fn an_http_error_keeps_its_status_and_body() {
        let failure = client(stub(Some(413)))
            .upload_metrics_classified(&MetricsBatch::new(Vec::new()))
            .unwrap_err();
        assert_eq!(failure.http_status(), Some(413));
        assert_eq!(failure.summary(), "HTTP 413: {\"error\":\"too large\"}");
    }

    #[test]
    fn a_refused_connection_is_a_network_error_not_a_timeout() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let failure = client(format!("http://127.0.0.1:{port}"))
            .upload_metrics_classified(&MetricsBatch::new(Vec::new()))
            .unwrap_err();
        assert_eq!(failure.kind(), MetricsFailureKind::Network, "{failure:?}");
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
