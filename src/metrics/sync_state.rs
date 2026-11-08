//! State of the usage-metrics upload queue, kept apart from authorship sync.
//!
//! The daemon replays the durable metrics queue on every tick. Before this
//! state existed it always sent the same 1,000-event batch, so a batch the
//! server could not finish within the client timeout timed out forever, and
//! the only record of why was a daemon log line. This file keeps:
//!
//! - the batch size to send next, halved after a timeout or an HTTP 413/504
//!   and grown back after a run of successes;
//! - an exponential backoff between failed attempts;
//! - the last failure (endpoint, kind, HTTP status, error, time, attempt
//!   count), which `autter doctor` and `autter sync status` show.
//!
//! Usage metrics are not attribution: authorship notes, commit summaries and
//! transcripts upload through their own queues, so a metrics failure is
//! reported here and never as "cloud sync is failing".

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api::metrics::{METRICS_UPLOAD_ENDPOINT, MetricsFailureKind, MetricsUploadFailure};
use crate::observability::MAX_METRICS_PER_ENVELOPE;

/// Largest batch the queue sends (the server's per-request event cap).
pub const MAX_BATCH: usize = MAX_METRICS_PER_ENVELOPE;
/// Successful batches in a row before the batch size doubles again.
pub const GROW_AFTER_SUCCESSES: u32 = 5;
/// First retry delay after a failure; doubles with each failure in a row.
pub const BACKOFF_BASE_SECS: i64 = 5;
/// Longest retry delay.
pub const BACKOFF_MAX_SECS: i64 = 15 * 60;

/// The last failed metrics upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsFailureRecord {
    /// Unix seconds.
    pub at: i64,
    pub endpoint: String,
    pub kind: MetricsFailureKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// Short error: "timeout after 30s (no response)", "HTTP 413: …".
    pub error: String,
    /// Events in the request that failed.
    pub batch_size: usize,
    /// Failed attempts in a row, including this one.
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsSyncState {
    /// Events to send in the next durable replay.
    pub batch_limit: usize,
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    /// Unix seconds before which the daemon does not try again.
    pub next_attempt_at: i64,
    pub last_failure: Option<MetricsFailureRecord>,
    pub last_success_at: Option<i64>,
    /// Events the server accepted since this state was created.
    pub uploaded_total: u64,
    /// Events dropped because the server can never accept them.
    pub dropped_total: u64,
    /// Failed upload attempts since this state was created.
    pub failed_attempts_total: u64,
}

impl Default for MetricsSyncState {
    fn default() -> Self {
        Self {
            batch_limit: MAX_BATCH,
            consecutive_failures: 0,
            consecutive_successes: 0,
            next_attempt_at: 0,
            last_failure: None,
            last_success_at: None,
            uploaded_total: 0,
            dropped_total: 0,
            failed_attempts_total: 0,
        }
    }
}

impl MetricsSyncState {
    pub fn path() -> PathBuf {
        crate::mdm::utils::home_dir()
            .join(".autter")
            .join("internal")
            .join("metrics-sync-state.json")
    }

    /// The saved state, or a fresh one if there is none or it is unreadable.
    pub fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = std::fs::write(path, bytes);
        }
    }

    /// Forget failures and batch-size reductions (`autter sync purge`).
    pub fn reset() {
        let _ = std::fs::remove_file(Self::path());
    }

    /// Events to request from the durable queue next.
    pub fn batch_limit(&self) -> usize {
        self.batch_limit.clamp(1, MAX_BATCH)
    }

    /// True while the daemon should wait before the next upload attempt.
    pub fn backoff_active(&self, now: i64) -> bool {
        now < self.next_attempt_at
    }

    /// The server answered for a batch: clear the failure, and after a run
    /// of successes try a bigger batch again.
    pub fn record_success(&mut self, now: i64, uploaded: usize, dropped: usize) {
        self.consecutive_failures = 0;
        self.next_attempt_at = 0;
        self.last_failure = None;
        self.last_success_at = Some(now);
        self.uploaded_total += uploaded as u64;
        self.dropped_total += dropped as u64;
        self.consecutive_successes += 1;
        if self.consecutive_successes >= GROW_AFTER_SUCCESSES && self.batch_limit() < MAX_BATCH {
            self.batch_limit = (self.batch_limit() * 2).min(MAX_BATCH);
            self.consecutive_successes = 0;
        }
    }

    /// A batch of `batch_size` events failed. Halve the batch when a smaller
    /// one could succeed, and wait longer after each failure in a row.
    pub fn record_failure(&mut self, now: i64, failure: &MetricsUploadFailure, batch_size: usize) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.consecutive_successes = 0;
        self.failed_attempts_total += 1;
        if failure.should_split() {
            let failed_size = batch_size.clamp(1, self.batch_limit());
            self.batch_limit = (failed_size / 2).max(1);
        }
        self.next_attempt_at = now + backoff_secs(self.consecutive_failures);
        self.last_failure = Some(MetricsFailureRecord {
            at: now,
            endpoint: METRICS_UPLOAD_ENDPOINT.to_string(),
            kind: failure.kind(),
            http_status: failure.http_status(),
            error: failure.summary(),
            batch_size,
            attempts: self.consecutive_failures,
        });
    }
}

/// Usage-metrics upload health for `autter doctor` and `autter sync status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageMetricsStatus {
    /// Events waiting in the local queue.
    pub queued: i64,
    /// Events uploaded from this computer since the state was created.
    pub uploaded: u64,
    /// Failed upload attempts since the state was created.
    pub failed_attempts: u64,
    /// Events dropped because the server can never accept them.
    pub dropped: u64,
    /// Events the next replay sends.
    pub batch_size: usize,
    /// Events are queued and the last attempt failed.
    pub failing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<MetricsFailureRecord>,
    pub last_success_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<i64>,
}

impl UsageMetricsStatus {
    pub fn current(queued: i64) -> Self {
        Self::from_state(&MetricsSyncState::load(), queued, unix_now())
    }

    /// A recorded failure only counts while events are waiting: once the
    /// queue is empty (drained, or purged), nothing is failing.
    pub fn from_state(state: &MetricsSyncState, queued: i64, now: i64) -> Self {
        let failing = queued > 0 && state.last_failure.is_some();
        Self {
            queued,
            uploaded: state.uploaded_total,
            failed_attempts: state.failed_attempts_total,
            dropped: state.dropped_total,
            batch_size: state.batch_limit(),
            failing,
            last_failure: state.last_failure.clone().filter(|_| failing),
            last_success_at: state.last_success_at,
            next_retry_at: (failing && state.next_attempt_at > now)
                .then_some(state.next_attempt_at),
        }
    }

    /// One line: counts, and the last error when failing.
    pub fn summary_line(&self) -> String {
        let mut line = format!(
            "Usage metrics: {} queued, {} uploaded, {}",
            self.queued,
            self.uploaded,
            failed_attempts_text(self.failed_attempts)
        );
        if self.dropped > 0 {
            line.push_str(&format!(", {} dropped as unacceptable", self.dropped));
        }
        if let Some(failure) = &self.last_failure {
            line.push_str(&format!(
                "\nLast metrics error: {}",
                describe_failure(failure)
            ));
            line.push_str(&format!(
                "\nMetrics fix: {}",
                advice(failure, self.batch_size)
            ));
        }
        line
    }
}

/// "1 failed attempt", "4 failed attempts".
pub fn failed_attempts_text(count: u64) -> String {
    if count == 1 {
        "1 failed attempt".to_string()
    } else {
        format!("{count} failed attempts")
    }
}

/// "POST /worker/metrics/upload: timeout after 30s (no response); 4 failed
/// attempts in a row (batch of 125 events), last at 2026-11-08 10:00".
pub fn describe_failure(failure: &MetricsFailureRecord) -> String {
    let attempts = if failure.attempts == 1 {
        "1 failed attempt".to_string()
    } else {
        format!("{} failed attempts in a row", failure.attempts)
    };
    format!(
        "POST {}: {}; {} (batch of {} events), last at {}",
        failure.endpoint,
        failure.error,
        attempts,
        failure.batch_size,
        crate::auth::format_unix_timestamp(failure.at)
    )
}

/// What to do about a metrics failure. Restarting the background service
/// never helps: the queue is durable and the daemon already retries.
pub fn advice(failure: &MetricsFailureRecord, next_batch: usize) -> String {
    match failure.kind {
        MetricsFailureKind::Timeout => format!(
            "nothing to restart; autter retries automatically in smaller batches (next: {next_batch} events). If it keeps timing out, check that your network or proxy lets requests to the autter API run for 30 seconds"
        ),
        MetricsFailureKind::TooLarge => format!(
            "nothing to restart; autter retries automatically in smaller batches (next: {next_batch} events)"
        ),
        MetricsFailureKind::Auth => {
            "the server refused this login for metrics; run `autter login`, then `autter doctor`"
                .to_string()
        }
        MetricsFailureKind::Network => {
            "check network or proxy access to the autter API; autter retries automatically"
                .to_string()
        }
        MetricsFailureKind::InvalidResponse => {
            "something other than the autter API answered (often a proxy or captive portal); check proxy settings. autter retries automatically"
                .to_string()
        }
        MetricsFailureKind::Server | MetricsFailureKind::Rejected => {
            "autter retries automatically; if this persists, send this output to autter support"
                .to_string()
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Delay before the next attempt after `failures` failures in a row:
/// 5s, 10s, 20s, … capped at 15 minutes.
pub fn backoff_secs(failures: u32) -> i64 {
    let exponent = failures.saturating_sub(1).min(16);
    (BACKOFF_BASE_SECS << exponent).min(BACKOFF_MAX_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeout() -> MetricsUploadFailure {
        MetricsUploadFailure::Timeout {
            after_secs: Some(30),
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_secs(1), 5);
        assert_eq!(backoff_secs(2), 10);
        assert_eq!(backoff_secs(3), 20);
        assert_eq!(backoff_secs(8), 640);
        assert_eq!(backoff_secs(9), BACKOFF_MAX_SECS);
        assert_eq!(backoff_secs(u32::MAX), BACKOFF_MAX_SECS);
    }

    #[test]
    fn timeouts_halve_the_batch_down_to_one() {
        let mut state = MetricsSyncState::default();
        let mut sizes = Vec::new();
        for _ in 0..12 {
            let size = state.batch_limit();
            state.record_failure(100, &timeout(), size);
            sizes.push(state.batch_limit());
        }
        assert_eq!(
            sizes,
            vec![500, 250, 125, 62, 31, 15, 7, 3, 1, 1, 1, 1],
            "each timeout halves the batch that failed"
        );
        let failure = state.last_failure.as_ref().unwrap();
        assert_eq!(failure.attempts, 12);
        assert_eq!(failure.kind, MetricsFailureKind::Timeout);
        assert_eq!(failure.endpoint, "/worker/metrics/upload");
        assert_eq!(failure.error, "timeout after 30s (no response)");
        assert!(state.backoff_active(100 + BACKOFF_MAX_SECS - 1));
        assert!(!state.backoff_active(100 + BACKOFF_MAX_SECS));
    }

    #[test]
    fn only_size_related_failures_split() {
        for (failure, splits) in [
            (timeout(), true),
            (
                MetricsUploadFailure::Http {
                    status: 413,
                    body: String::new(),
                },
                true,
            ),
            (
                MetricsUploadFailure::Http {
                    status: 504,
                    body: String::new(),
                },
                true,
            ),
            (
                MetricsUploadFailure::Http {
                    status: 500,
                    body: String::new(),
                },
                false,
            ),
            (
                MetricsUploadFailure::Http {
                    status: 401,
                    body: String::new(),
                },
                false,
            ),
            (
                MetricsUploadFailure::Network {
                    message: "connection refused".to_string(),
                },
                false,
            ),
        ] {
            let mut state = MetricsSyncState::default();
            state.record_failure(0, &failure, MAX_BATCH);
            assert_eq!(
                state.batch_limit() < MAX_BATCH,
                splits,
                "{failure:?} split = {splits}"
            );
            assert_eq!(state.next_attempt_at, BACKOFF_BASE_SECS);
        }
    }

    #[test]
    fn success_clears_the_failure_and_grows_back_slowly() {
        let mut state = MetricsSyncState::default();
        state.record_failure(0, &timeout(), 1000);
        state.record_failure(5, &timeout(), 500);
        assert_eq!(state.batch_limit(), 250);

        state.record_success(20, 250, 0);
        assert!(state.last_failure.is_none());
        assert_eq!(state.consecutive_failures, 0);
        assert!(!state.backoff_active(20));
        assert_eq!(state.batch_limit(), 250, "one success does not grow");

        for _ in 1..GROW_AFTER_SUCCESSES {
            state.record_success(21, 250, 0);
        }
        assert_eq!(state.batch_limit(), 500);
        assert_eq!(state.uploaded_total, 250 * GROW_AFTER_SUCCESSES as u64);
    }

    #[test]
    fn older_or_partial_files_still_load() {
        let state: MetricsSyncState = serde_json::from_str(r#"{"batch_limit":40}"#).unwrap();
        assert_eq!(state.batch_limit(), 40);
        assert!(state.last_failure.is_none());
        let state: MetricsSyncState = serde_json::from_str(r#"{"batch_limit":0}"#).unwrap();
        assert_eq!(state.batch_limit(), 1);
    }
}
