//! State of the usage-metrics upload queue, kept apart from authorship sync.
//!
//! The daemon replays the durable metrics queue on every tick. Before this
//! state existed it always sent the same 1,000-event batch, so a batch the
//! server could not finish within the client timeout timed out forever, and
//! the only record of why was a daemon log line. This file keeps:
//!
//! - the batch size to send next, halved after a timeout, an HTTP 413/504 or
//!   a "retry later" answer, and grown back after a run of successes;
//! - an exponential backoff between failed attempts;
//! - the batch that keeps failing the same way, so a server error that is
//!   tied to particular rows is narrowed down to those rows, which are then
//!   set aside (kept, not deleted) so the rest of the queue drains;
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
/// Identical failures on the same batch before it is halved to find the rows
/// the server fails on. A lone event must also fail this many times on its
/// own before it is set aside.
pub const ISOLATE_AFTER_FAILURES: u32 = 3;

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

/// A batch that keeps failing the same way: the queue row it starts at, what
/// the server answered, and how many times in a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepeatedFailure {
    /// Queue id of the first event in the failed request.
    pub head_id: i64,
    /// Failure kind and HTTP status (not the body, which can vary).
    pub signature: String,
    pub count: u32,
    /// The batch is being halved to find the events the server fails on.
    #[serde(default)]
    pub isolating: bool,
}

/// What the queue should do with a batch that just failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedBatchAction {
    /// Keep the events queued and try again after the backoff.
    Retry,
    /// The batch is a single event that fails the same way every time: set it
    /// aside so the events behind it can upload.
    SetAside,
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
    /// The batch that keeps failing the same way, if one does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeated_failure: Option<RepeatedFailure>,
    /// Events set aside since this state was created because the server
    /// failed on them every time. They stay in the local queue database.
    pub set_aside_total: u64,
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
            repeated_failure: None,
            set_aside_total: 0,
        }
    }
}

impl MetricsSyncState {
    pub fn path() -> PathBuf {
        Self::path_in(
            &crate::mdm::utils::home_dir()
                .join(".autter")
                .join("internal"),
        )
    }

    /// The state file inside `internal_dir` (`~/.autter/internal`).
    pub fn path_in(internal_dir: &std::path::Path) -> PathBuf {
        internal_dir.join("metrics-sync-state.json")
    }

    /// The saved state, or a fresh one if there is none or it is unreadable.
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        self.save_to(&Self::path());
    }

    pub fn save_to(&self, path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = std::fs::write(path, bytes);
        }
    }

    /// Forget failures and batch-size reductions. Only `autter sync purge`
    /// does this: another queue recovering says nothing about this one.
    pub fn reset() {
        let _ = std::fs::remove_file(Self::path());
    }

    /// Events to request from the durable queue next.
    pub fn batch_limit(&self) -> usize {
        self.batch_limit.clamp(1, MAX_BATCH)
    }

    /// True while the daemon should wait before the next upload attempt.
    /// Call [`Self::clamp_backoff`] first so a clock that stepped backward
    /// cannot stretch the wait.
    pub fn backoff_active(&self, now: i64) -> bool {
        now < self.next_attempt_at
    }

    /// The longest the queue may still have to wait at `now`: the delay that
    /// was set after the last failure (never more than [`BACKOFF_MAX_SECS`]).
    fn longest_wait(&self) -> i64 {
        backoff_secs(self.consecutive_failures.max(1))
    }

    /// Bound the wait to the backoff that was asked for, measured from `now`.
    /// `next_attempt_at` is a wall-clock time, so when the system clock steps
    /// backward (NTP correction, a VM resume, a manual change) it can be hours
    /// or years ahead of the new clock and the queue would not retry until
    /// then. Returns true when the stored time changed and should be saved.
    pub fn clamp_backoff(&mut self, now: i64) -> bool {
        let latest = now.saturating_add(self.longest_wait());
        if self.next_attempt_at > latest {
            self.next_attempt_at = latest;
            true
        } else {
            false
        }
    }

    /// When the next attempt is due, as shown to the user at `now`.
    fn next_attempt_at_clamped(&self, now: i64) -> i64 {
        self.next_attempt_at
            .min(now.saturating_add(self.longest_wait()))
    }

    /// The server answered for a batch: clear the failure, and after a run
    /// of successes try a bigger batch again.
    pub fn record_success(&mut self, now: i64, uploaded: usize) {
        self.consecutive_failures = 0;
        self.next_attempt_at = 0;
        self.last_failure = None;
        self.repeated_failure = None;
        self.last_success_at = Some(now);
        self.uploaded_total += uploaded as u64;
        self.consecutive_successes += 1;
        if self.consecutive_successes >= GROW_AFTER_SUCCESSES && self.batch_limit() < MAX_BATCH {
            self.batch_limit = (self.batch_limit() * 2).min(MAX_BATCH);
            self.consecutive_successes = 0;
        }
    }

    /// The server wrote part of a batch of `batch_size` events and answered
    /// "retry later" for the rest: it ran out of time. That is progress, so
    /// the failure is cleared, but the next batch is half the size and the
    /// run of successes starts again.
    pub fn record_partial_success(&mut self, now: i64, uploaded: usize, batch_size: usize) {
        self.record_success(now, uploaded);
        self.halve_batch(batch_size);
        self.consecutive_successes = 0;
    }

    /// Events were deleted from the queue because the server can never accept
    /// them. Counted whatever else happened in the same pass.
    pub fn record_dropped(&mut self, dropped: usize) {
        self.dropped_total += dropped as u64;
    }

    /// A batch of `batch_size` events failed. Halve the batch when a smaller
    /// one could succeed, and wait longer after each failure in a row.
    pub fn record_failure(&mut self, now: i64, failure: &MetricsUploadFailure, batch_size: usize) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.consecutive_successes = 0;
        self.failed_attempts_total += 1;
        if failure.should_split() {
            self.halve_batch(batch_size);
        }
        self.next_attempt_at = now.saturating_add(backoff_secs(self.consecutive_failures));
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

    /// [`Self::record_failure`] for a request whose first queued event is
    /// `head_id`, which also notices a batch that fails the same way every
    /// time.
    ///
    /// A server error that depends on the rows (an HTTP 500 on the same
    /// events every time) would otherwise be retried forever at full size,
    /// and nothing behind those rows would ever upload. After
    /// [`ISOLATE_AFTER_FAILURES`] identical failures the batch is halved on
    /// each further one, down to a single event; a single event that has
    /// failed that many times on its own is to be set aside.
    pub fn record_batch_failure(
        &mut self,
        now: i64,
        failure: &MetricsUploadFailure,
        batch_size: usize,
        head_id: i64,
    ) -> FailedBatchAction {
        self.record_failure(now, failure, batch_size);
        if !failure.may_depend_on_rows() {
            self.repeated_failure = None;
            return FailedBatchAction::Retry;
        }
        let signature = failure.signature();
        let mut repeated = match self.repeated_failure.take() {
            Some(repeated) if repeated.head_id == head_id && repeated.signature == signature => {
                RepeatedFailure {
                    count: repeated.count.saturating_add(1),
                    ..repeated
                }
            }
            _ => RepeatedFailure {
                head_id,
                signature,
                count: 1,
                isolating: false,
            },
        };
        let action = if repeated.count < ISOLATE_AFTER_FAILURES {
            FailedBatchAction::Retry
        } else if batch_size <= 1 {
            FailedBatchAction::SetAside
        } else {
            // The same rows got the same answer again: look at half of them
            // next. A lone event still has to fail on its own that many
            // times before it is set aside.
            self.halve_batch(batch_size);
            repeated.isolating = true;
            if self.batch_limit() == 1 {
                repeated.count = 0;
            }
            FailedBatchAction::Retry
        };
        if repeated.isolating {
            // Each request is now smaller than the last, so the wait stops
            // growing while the failing rows are narrowed down.
            let wait =
                backoff_secs(self.consecutive_failures).min(backoff_secs(ISOLATE_AFTER_FAILURES));
            self.next_attempt_at = now.saturating_add(wait);
        }
        self.repeated_failure = Some(repeated);
        action
    }

    /// The failing event was moved out of the queue. The run of failures and
    /// the wait that the last one set are kept: the events behind it are
    /// tried after that wait, and if the server is failing on everything (an
    /// outage, not these rows) the waits keep growing, so few events are set
    /// aside before it recovers.
    pub fn record_set_aside(&mut self, count: usize) {
        self.set_aside_total += count as u64;
        self.repeated_failure = None;
    }

    fn halve_batch(&mut self, failed_size: usize) {
        let failed_size = failed_size.clamp(1, self.batch_limit());
        self.batch_limit = (failed_size / 2).max(1);
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
    /// Events set aside on this computer because the server failed on them
    /// every time. They are kept, not uploaded and not deleted, until
    /// `autter sync retry` queues them again or `autter sync purge` deletes
    /// them.
    pub set_aside: i64,
    /// The error the most recent set-aside event failed with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_aside_reason: Option<String>,
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
        let (set_aside, reason) = crate::metrics::db::MetricsDatabase::global()
            .ok()
            .and_then(|db| {
                let db = db.lock().ok()?;
                Some((
                    db.count_set_aside().ok()?,
                    db.last_set_aside_reason().ok().flatten(),
                ))
            })
            .unwrap_or((0, None));
        Self::from_state(&MetricsSyncState::load(), queued, unix_now())
            .with_set_aside(set_aside as i64, reason)
    }

    /// The same status with the set-aside events counted (they live in the
    /// queue database, not in the state file).
    pub fn with_set_aside(mut self, set_aside: i64, reason: Option<String>) -> Self {
        self.set_aside = set_aside;
        self.set_aside_reason = reason.filter(|_| set_aside > 0);
        self
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
            set_aside: 0,
            set_aside_reason: None,
            batch_size: state.batch_limit(),
            failing,
            last_failure: state.last_failure.clone().filter(|_| failing),
            last_success_at: state.last_success_at,
            next_retry_at: (failing && state.next_attempt_at > now)
                .then(|| state.next_attempt_at_clamped(now)),
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
        if self.set_aside > 0 {
            line.push_str(&format!(", {} set aside", self.set_aside));
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
        if self.set_aside > 0 {
            line.push_str(&format!("\n{}", set_aside_text(self.set_aside)));
            if let Some(reason) = &self.set_aside_reason {
                line.push_str(&format!("\nSet aside after: {reason}"));
            }
        }
        line
    }
}

/// What "set aside" means and what to do about it, for `autter doctor` and
/// `autter sync status`.
pub fn set_aside_text(count: i64) -> String {
    format!(
        "{count} usage-metrics {} set aside: the server failed on {} every time, so {} kept on this computer and the rest of the queue uploads. Queue {} again: `{RETRY_COMMAND}`. Delete {}: `{}`.",
        if count == 1 { "event is" } else { "events are" },
        if count == 1 { "it" } else { "them" },
        if count == 1 { "it is" } else { "they are" },
        if count == 1 { "it" } else { "them" },
        if count == 1 { "it" } else { "them" },
        crate::upload_hold::PURGE_COMMAND,
    )
}

/// Puts set-aside usage-metrics events back in the upload queue.
pub const RETRY_COMMAND: &str = "autter sync retry";

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

        state.record_success(20, 250);
        assert!(state.last_failure.is_none());
        assert_eq!(state.consecutive_failures, 0);
        assert!(!state.backoff_active(20));
        assert_eq!(state.batch_limit(), 250, "one success does not grow");

        for _ in 1..GROW_AFTER_SUCCESSES {
            state.record_success(21, 250);
        }
        assert_eq!(state.batch_limit(), 500);
        assert_eq!(state.uploaded_total, 250 * GROW_AFTER_SUCCESSES as u64);
    }

    fn http(status: u16) -> MetricsUploadFailure {
        MetricsUploadFailure::Http {
            status,
            body: r#"{"error":"Data-plane operation failed"}"#.to_string(),
        }
    }

    /// The system clock stepped back a day after a failure. The stored retry
    /// time is then a day ahead of the clock; before, the queue did not try
    /// again until the clock caught up.
    #[test]
    fn a_clock_that_steps_backward_cannot_stretch_the_wait() {
        let failed_at = 1_762_588_800;
        let mut state = MetricsSyncState::default();
        state.record_failure(failed_at, &timeout(), 1000);
        state.record_failure(failed_at, &timeout(), 500);
        assert_eq!(state.next_attempt_at, failed_at + 10);

        let now = failed_at - 86_400;
        assert!(state.clamp_backoff(now), "the stored time is out of range");
        assert_eq!(
            state.next_attempt_at,
            now + 10,
            "the wait is the backoff that was asked for, from the new clock"
        );
        assert!(state.backoff_active(now + 9));
        assert!(!state.backoff_active(now + 10));
        assert!(!state.clamp_backoff(now + 3), "already in range: no change");

        // Whatever the stored value, the wait never exceeds the maximum.
        for failures in [1, 5, 40] {
            let mut state = MetricsSyncState {
                consecutive_failures: failures,
                next_attempt_at: i64::MAX,
                ..MetricsSyncState::default()
            };
            state.clamp_backoff(now);
            assert!(state.next_attempt_at - now <= BACKOFF_MAX_SECS);
            assert_eq!(state.next_attempt_at - now, backoff_secs(failures));
        }

        // A clock that moves forward, or not at all, changes nothing.
        let mut state = MetricsSyncState::default();
        state.record_failure(failed_at, &timeout(), 1000);
        assert!(!state.clamp_backoff(failed_at));
        assert!(!state.clamp_backoff(failed_at + 3));
        assert_eq!(state.next_attempt_at, failed_at + BACKOFF_BASE_SECS);
    }

    #[test]
    fn the_status_never_shows_a_retry_further_away_than_the_backoff() {
        let mut state = MetricsSyncState::default();
        state.record_failure(1_000_000, &timeout(), 1000);
        let status = UsageMetricsStatus::from_state(&state, 10, 1_000);
        assert_eq!(status.next_retry_at, Some(1_000 + BACKOFF_BASE_SECS));
    }

    /// HTTP 500 on the same batch every time: after three identical failures
    /// the batch is halved on each further one, and a lone event that fails
    /// three times on its own is to be set aside.
    #[test]
    fn the_same_server_error_on_the_same_batch_narrows_it_down_to_one_event() {
        let mut state = MetricsSyncState::default();
        let mut now = 0;
        let mut sizes = Vec::new();
        let mut verdict = FailedBatchAction::Retry;
        for _ in 0..40 {
            let size = state.batch_limit();
            sizes.push(size);
            verdict = state.record_batch_failure(now, &http(500), size, 7);
            if verdict == FailedBatchAction::SetAside {
                break;
            }
            assert!(state.next_attempt_at > now, "every failure waits");
            now = state.next_attempt_at;
        }
        assert_eq!(verdict, FailedBatchAction::SetAside);
        assert_eq!(
            sizes,
            vec![1000, 1000, 1000, 500, 250, 125, 62, 31, 15, 7, 3, 1, 1, 1],
            "three tries at full size, then halved to one, then three tries alone"
        );
        assert!(
            now <= 5 + 10 + 20 * 11,
            "the wait stops growing while the batch is narrowed down: {now}s"
        );

        state.record_set_aside(1);
        assert_eq!(state.set_aside_total, 1);
        assert!(state.repeated_failure.is_none());
        assert!(
            state.next_attempt_at - now <= backoff_secs(ISOLATE_AFTER_FAILURES),
            "the events behind it are tried after a short wait"
        );
    }

    #[test]
    fn a_different_batch_or_a_different_error_starts_the_count_again() {
        let mut state = MetricsSyncState::default();
        state.record_batch_failure(0, &http(500), 1000, 7);
        state.record_batch_failure(0, &http(500), 1000, 7);
        // Another first row: the earlier rows uploaded or were re-queued.
        state.record_batch_failure(0, &http(500), 1000, 8);
        assert_eq!(state.batch_limit(), 1000);
        // Another status.
        state.record_batch_failure(0, &http(500), 1000, 8);
        state.record_batch_failure(0, &http(400), 1000, 8);
        assert_eq!(state.batch_limit(), 1000);
        assert_eq!(state.repeated_failure.as_ref().unwrap().count, 1);
        // A success forgets it.
        state.record_success(1, 10);
        assert!(state.repeated_failure.is_none());
    }

    /// Failures that say nothing about the events never isolate or set aside:
    /// the login, the rate, a gateway, the network, a timeout.
    #[test]
    fn failures_that_are_not_about_the_events_never_set_anything_aside() {
        for failure in [
            http(401),
            http(403),
            http(408),
            http(429),
            http(502),
            http(503),
            MetricsUploadFailure::Network {
                message: "connection refused".to_string(),
            },
            MetricsUploadFailure::InvalidResponse {
                message: "expected value".to_string(),
            },
        ] {
            let mut state = MetricsSyncState::default();
            for _ in 0..30 {
                let size = state.batch_limit();
                assert_eq!(
                    state.record_batch_failure(0, &failure, size, 7),
                    FailedBatchAction::Retry,
                    "{failure:?}"
                );
            }
            assert_eq!(state.batch_limit(), 1000, "{failure:?}");
            assert!(state.repeated_failure.is_none(), "{failure:?}");
        }
        // A timeout halves (a smaller batch finishes sooner) but a lone event
        // that times out is kept in the queue.
        let mut state = MetricsSyncState::default();
        for _ in 0..30 {
            let size = state.batch_limit();
            assert_eq!(
                state.record_batch_failure(0, &timeout(), size, 7),
                FailedBatchAction::Retry
            );
        }
        assert_eq!(state.batch_limit(), 1);
    }

    #[test]
    fn a_partial_answer_halves_the_batch_without_counting_as_a_failure() {
        let mut state = MetricsSyncState::default();
        state.record_partial_success(10, 400, 1000);
        assert_eq!(state.batch_limit(), 500);
        assert_eq!(state.uploaded_total, 400);
        assert!(state.last_failure.is_none());
        assert!(!state.backoff_active(10));
        assert_eq!(state.failed_attempts_total, 0);
        // It takes a full run of successes to grow back.
        for _ in 1..GROW_AFTER_SUCCESSES {
            state.record_success(11, 500);
        }
        assert_eq!(state.batch_limit(), 500);
        state.record_success(11, 500);
        assert_eq!(state.batch_limit(), 1000);
    }

    #[test]
    fn set_aside_events_are_reported_with_what_to_do() {
        let status = UsageMetricsStatus::from_state(&MetricsSyncState::default(), 0, 0)
            .with_set_aside(2, Some("HTTP 500".to_string()));
        let line = status.summary_line();
        assert!(line.contains("2 set aside"), "{line}");
        assert!(line.contains("autter sync retry"), "{line}");
        assert!(line.contains("Set aside after: HTTP 500"), "{line}");
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["set_aside"], 2);
        assert_eq!(json["set_aside_reason"], "HTTP 500");
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
