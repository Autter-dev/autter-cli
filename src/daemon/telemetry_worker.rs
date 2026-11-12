//! Daemon-side telemetry worker that batches and dispatches events.
//!
//! Runs inside the daemon process using tokio. Accumulates telemetry envelopes
//! and CAS payloads, then flushes them to their destinations every 3 seconds.

use crate::api::metrics::{METRICS_UPLOAD_ENDPOINT, MetricsUploadFailure, MetricsUploadResponse};
use crate::api::{ApiClient, ApiContext, CasObject, CasUploadRequest};
use crate::config::{Config, get_or_create_distinct_id};
use crate::daemon::control_api::{CasSyncPayload, TelemetryEnvelope};
use crate::metrics::db::MetricsDatabase;
use crate::metrics::sync_state::{FailedBatchAction, MetricsSyncState};
use crate::metrics::{MetricEvent, MetricsBatch};
use crate::observability::split_metrics_envelopes;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, interval};

const FLUSH_INTERVAL: Duration = Duration::from_secs(3);

/// Accumulated telemetry events waiting to be flushed.
struct TelemetryBuffer {
    errors: Vec<ErrorEvent>,
    performances: Vec<PerformanceEvent>,
    messages: Vec<MessageEvent>,
    metrics: Vec<MetricEvent>,
    cas_records: Vec<CasSyncPayload>,
}

struct ErrorEvent {
    timestamp: String,
    message: String,
    context: Option<Value>,
}

struct PerformanceEvent {
    timestamp: String,
    operation: String,
    duration_ms: u128,
    context: Option<Value>,
    tags: Option<std::collections::HashMap<String, String>>,
}

struct MessageEvent {
    timestamp: String,
    message: String,
    level: String,
    context: Option<Value>,
}

impl TelemetryBuffer {
    fn new() -> Self {
        Self {
            errors: Vec::new(),
            performances: Vec::new(),
            messages: Vec::new(),
            metrics: Vec::new(),
            cas_records: Vec::new(),
        }
    }

    fn ingest_envelopes(&mut self, envelopes: Vec<TelemetryEnvelope>) {
        for envelope in envelopes {
            match envelope {
                TelemetryEnvelope::Error {
                    timestamp,
                    message,
                    context,
                } => {
                    self.errors.push(ErrorEvent {
                        timestamp,
                        message,
                        context,
                    });
                }
                TelemetryEnvelope::Performance {
                    timestamp,
                    operation,
                    duration_ms,
                    context,
                    tags,
                } => {
                    self.performances.push(PerformanceEvent {
                        timestamp,
                        operation,
                        duration_ms,
                        context,
                        tags,
                    });
                }
                TelemetryEnvelope::Message {
                    timestamp,
                    message,
                    level,
                    context,
                } => {
                    self.messages.push(MessageEvent {
                        timestamp,
                        message,
                        level,
                        context,
                    });
                }
                TelemetryEnvelope::Metrics { events } => {
                    self.metrics.extend(events);
                }
            }
        }
    }

    fn ingest_cas(&mut self, records: Vec<CasSyncPayload>) {
        self.cas_records.extend(records);
    }

    fn take(&mut self) -> TelemetryBuffer {
        TelemetryBuffer {
            errors: std::mem::take(&mut self.errors),
            performances: std::mem::take(&mut self.performances),
            messages: std::mem::take(&mut self.messages),
            metrics: std::mem::take(&mut self.metrics),
            cas_records: std::mem::take(&mut self.cas_records),
        }
    }
}

/// Handle for submitting telemetry directly within the daemon process.
#[derive(Clone)]
pub struct DaemonTelemetryWorkerHandle {
    buffer: Arc<Mutex<TelemetryBuffer>>,
}

impl DaemonTelemetryWorkerHandle {
    #[cfg(test)]
    pub fn new_noop() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(TelemetryBuffer::new())),
        }
    }

    /// Submit telemetry envelopes for batched processing.
    pub async fn submit_telemetry(&self, envelopes: Vec<TelemetryEnvelope>) {
        self.buffer.lock().await.ingest_envelopes(envelopes);
    }

    /// Submit from a blocking worker without routing through global diagnostic
    /// logging, which is best-effort and disabled in test-support builds.
    pub fn submit_telemetry_blocking(&self, envelopes: Vec<TelemetryEnvelope>) {
        self.buffer.blocking_lock().ingest_envelopes(envelopes);
    }

    /// Submit CAS records for batched upload.
    pub async fn submit_cas(&self, records: Vec<CasSyncPayload>) {
        self.buffer.lock().await.ingest_cas(records);
    }

    /// Returns the current number of buffered metric events.
    ///
    /// Used by the transcript worker for backpressure: if the buffer is
    /// above a threshold, the worker yields to let the flush loop drain it.
    /// Returns `usize::MAX` when the lock is contended, so callers default
    /// to "wait" rather than "push more".
    pub fn metrics_buffer_len(&self) -> usize {
        self.buffer
            .try_lock()
            .map(|buf| buf.metrics.len())
            .unwrap_or(usize::MAX)
    }

    /// Submit telemetry envelopes synchronously (best-effort, non-blocking).
    ///
    /// Used by the daemon process's own `observability::log_*()` calls which
    /// cannot go through the control socket (the daemon can't connect to itself).
    /// Uses `try_lock()` to avoid blocking the caller if the buffer is contested.
    pub fn submit_telemetry_sync(&self, envelopes: Vec<TelemetryEnvelope>) {
        if let Ok(mut buf) = self.buffer.try_lock() {
            buf.ingest_envelopes(envelopes);
        }
    }

    /// Submit CAS records synchronously (best-effort, non-blocking).
    ///
    /// Used by daemon-owned post-commit paths that cannot route through the
    /// control socket because the daemon cannot connect to itself.
    pub fn submit_cas_sync(&self, records: Vec<CasSyncPayload>) {
        if let Ok(mut buf) = self.buffer.try_lock() {
            buf.ingest_cas(records);
        }
    }
}

/// Global handle for the daemon's in-process telemetry worker.
///
/// Set once when the daemon spawns its telemetry worker, allowing
/// `observability::log_*()` functions to route events directly into
/// the worker buffer when running inside the daemon process.
static DAEMON_INTERNAL_TELEMETRY: std::sync::OnceLock<DaemonTelemetryWorkerHandle> =
    std::sync::OnceLock::new();

/// Register the daemon's in-process telemetry worker handle.
/// Called once during daemon startup after `spawn_telemetry_worker()`.
pub fn set_daemon_internal_telemetry(handle: DaemonTelemetryWorkerHandle) {
    let _ = DAEMON_INTERNAL_TELEMETRY.set(handle);
}

/// Submit telemetry from within the daemon process.
/// Returns true if the handle was available and envelopes were submitted.
pub fn submit_daemon_internal_telemetry(envelopes: Vec<TelemetryEnvelope>) -> bool {
    if let Some(handle) = DAEMON_INTERNAL_TELEMETRY.get() {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let handle = handle.clone();
            runtime.spawn(async move {
                handle.submit_telemetry(envelopes).await;
            });
        } else {
            handle.submit_telemetry_sync(envelopes);
        }
        true
    } else {
        false
    }
}

/// Submit CAS records from within the daemon process (sync, best-effort).
/// Returns true if the handle was available and records were submitted.
pub fn submit_daemon_internal_cas(records: Vec<CasSyncPayload>) -> bool {
    if let Some(handle) = DAEMON_INTERNAL_TELEMETRY.get() {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let handle = handle.clone();
            runtime.spawn(async move {
                handle.submit_cas(records).await;
            });
        } else {
            handle.submit_cas_sync(records);
        }
        true
    } else {
        false
    }
}

/// Spawn the telemetry worker task. Returns a handle for submitting events.
///
/// The worker runs a flush loop every 3 seconds, sending accumulated events
/// to their respective destinations (Sentry, PostHog, metrics API, CAS API).
pub fn spawn_telemetry_worker() -> DaemonTelemetryWorkerHandle {
    let buffer = Arc::new(Mutex::new(TelemetryBuffer::new()));
    let handle = DaemonTelemetryWorkerHandle {
        buffer: buffer.clone(),
    };

    tokio::spawn(async move {
        telemetry_flush_loop(buffer).await;
    });

    handle
}

async fn telemetry_flush_loop(buffer: Arc<Mutex<TelemetryBuffer>>) {
    let mut ticker = interval(FLUSH_INTERVAL);
    // The first tick completes immediately; skip it.
    ticker.tick().await;

    loop {
        ticker.tick().await;

        // Take whatever in-memory telemetry accumulated this tick (possibly
        // nothing). The flush must still run on an empty buffer: the durable
        // queues (CAS transcripts, authorship notes, file-change aggregates)
        // are drained inside flush_telemetry_batch, and gating them on
        // in-memory activity left them stranded whenever the daemon was
        // otherwise idle.
        let snapshot = {
            let mut buf = buffer.lock().await;
            buf.take()
        };

        // Flush in a blocking task since the underlying HTTP clients are synchronous.
        // Catch a panic inside the flush so its message is reported: the join handle
        // only surfaces "task panicked", which hides the real cause.
        tokio::task::spawn_blocking(move || {
            if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                flush_telemetry_batch(snapshot);
            })) {
                tracing::error!("telemetry flush panicked: {}", panic_message(&panic));
            }
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!(%e, "telemetry flush task panicked");
        });
    }
}

/// Extract a human-readable message from a caught panic payload.
///
/// Takes the `Box` rather than a `&(dyn Any + Send)`: materializing that
/// reference replaces the object's vtable with the trait-object one, after
/// which `downcast_ref` no longer sees the concrete payload type and every
/// message comes back as "unknown panic".
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn flush_telemetry_batch(batch: TelemetryBuffer) {
    let config = Config::get();

    // Flush metrics (uploaded or stored in SQLite for retry; dropped in local mode)
    if !batch.metrics.is_empty() {
        flush_metrics(&batch.metrics);
    }

    // Flush Sentry events (errors, performance, messages)
    let has_sentry_or_posthog =
        !batch.errors.is_empty() || !batch.performances.is_empty() || !batch.messages.is_empty();

    if has_sentry_or_posthog {
        let distinct_id = get_or_create_distinct_id();
        flush_sentry_and_posthog(
            config,
            &distinct_id,
            &batch.errors,
            &batch.performances,
            &batch.messages,
        );
    }

    // Flush CAS records submitted in-memory (e.g. daemon-internal stream worker).
    if !batch.cas_records.is_empty() {
        flush_cas(batch.cas_records);
    }

    // Drain the durable queues. Skipped while the auth backoff is active so an
    // expired login doesn't trigger a token-refresh network call on every tick,
    // and until the one-time hold of the pre-upgrade backlog has been applied
    // (held items are never dequeued; see crate::upload_hold).
    let legacy_backlog_held = match crate::upload_hold::ensure_legacy_migration() {
        Ok(done) => done,
        Err(error) => {
            tracing::warn!(%error, "upload hold: legacy backlog migration failed; skipping durable uploads");
            false
        }
    };
    if legacy_backlog_held && !durable_sync_auth_backoff_active() {
        // Metrics that failed while offline or while the org database was
        // unavailable must be replayed automatically. Historically this queue
        // was only drained by a hidden manual command, which could leave users
        // with gigabytes of telemetry that never reached the dashboard.
        flush_stored_metrics();

        // Drain the durable CAS queue (the post-commit transcript bridge enqueues
        // here). This reads directly from the internal DB, mirroring flush_notes.
        flush_cas_queue();

        // Flush pending notes (reads directly from notes-db; no-op when kind != Http).
        flush_notes();

        // Commit-level provenance summaries are a separate, query-friendly
        // org-database projection. They are queued before the post-commit path
        // attempts a live upload, then replayed here until the upsert succeeds.
        flush_commit_summaries();

        // Flush pending file change aggregates to the org database.
        crate::file_changes::flush_pending_to_cloud();
    }
}

/// Drain a bounded batch of durable commit provenance summaries.
fn flush_commit_summaries() {
    use crate::api::types::CommitAuthorshipSummary;

    // Local mode never uploads; leave any queued rows untouched so they only
    // leave the machine if the user reconnects and agrees to import them.
    if !crate::config::platform_sync_enabled_now() {
        return;
    }

    let db = match crate::notes::db::NotesDatabase::global() {
        Ok(db) => db,
        Err(error) => {
            tracing::warn!(%error, "commit summaries: failed to open durable queue");
            return;
        }
    };
    let pending = db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .count_pending_commit_summaries()
        .unwrap_or(0);
    if pending == 0 {
        return;
    }

    // Check the home token before claiming queue rows. Repo-specific rows may
    // mint a different org token below, but no hosted upload is possible when
    // the base login itself is unavailable.
    let home_client = ApiClient::new(ApiContext::new(None));
    if !home_client.is_logged_in() && !home_client.has_api_key() {
        note_durable_sync_unauthenticated("commit summaries", pending);
        if let Some(issue) = home_client.auth_issue_to_log() {
            tracing::warn!(reason = %issue, pending, "commit summaries: sync authentication unavailable");
        }
        return;
    }

    let rows = match db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .dequeue_commit_summaries(50)
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "commit summaries: failed to dequeue durable rows");
            return;
        }
    };

    for pending_row in rows {
        let row = match serde_json::from_str::<CommitAuthorshipSummary>(&pending_row.payload) {
            Ok(row) => row,
            Err(error) => {
                let mut lock = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let _ = lock.mark_commit_summary_failed(
                    &pending_row.commit_sha,
                    &format!("invalid queued payload: {error}"),
                );
                tracing::warn!(commit_sha = %pending_row.commit_sha, %error, "commit summaries: invalid queued payload retained");
                continue;
            }
        };

        // Only upload to an org that connected this repo; otherwise keep it here.
        let client = match crate::api::client::route_for_repo(row.repo_url.as_deref()) {
            crate::api::client::RepoRoute::Upload(client) => client,
            crate::api::client::RepoRoute::NotConnected => {
                let mut lock = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let _ = lock.defer_unconnected_commit_summary(&pending_row.commit_sha);
                continue;
            }
            crate::api::client::RepoRoute::Unavailable(reason) => {
                let mut lock = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let _ = lock.mark_commit_summary_failed(&pending_row.commit_sha, &reason);
                continue;
            }
        };

        let result = client.upload_commit_authorship_summary(&row, &get_or_create_distinct_id());
        let mut lock = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match result {
            Ok(()) => {
                if let Err(error) = lock.mark_commit_summary_synced(&pending_row.commit_sha) {
                    tracing::warn!(commit_sha = %pending_row.commit_sha, %error, "commit summaries: uploaded row remains queued");
                } else {
                    note_durable_sync_authenticated();
                    crate::auth::notice::clear_upload_failure_for("commit_summaries");
                }
            }
            Err(error) if crate::api::data_plane::is_repo_not_connected(&error) => {
                // The server knows better than our cached answer: park it.
                let _ = lock.defer_unconnected_commit_summary(&pending_row.commit_sha);
            }
            Err(error) => {
                let _ =
                    lock.mark_commit_summary_failed(&pending_row.commit_sha, &error.to_string());
                note_durable_sync_upload_failed("commit_summaries", &error.to_string());
                tracing::warn!(commit_sha = %pending_row.commit_sha, %error, "commit summaries: upload failed; retained for retry");
            }
        }
    }
}

// ----- Durable-queue auth backoff -------------------------------------------
//
// The durable queues (CAS transcripts, authorship notes, file-change
// aggregates) are drained on every flush tick, even when no in-memory
// telemetry accumulated. Each drain attempt can trigger a token-refresh
// network call, so after an unauthenticated attempt we back off instead of
// retrying every 3 seconds — and emit a rate-limited warning so a stalled
// sync is visible in the daemon log. (This used to be a debug-level message,
// which let queued transcripts and notes sit pending for weeks unnoticed
// after a login expired.)

/// Unix timestamp before which auth-gated durable-queue flushes are skipped.
static DURABLE_SYNC_AUTH_RETRY_AFTER: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);
/// Unix timestamp of the last "sync blocked" warning, to rate-limit it.
static DURABLE_SYNC_LAST_AUTH_WARN: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);
const DURABLE_SYNC_AUTH_RETRY_SECS: i64 = 60;
const DURABLE_SYNC_AUTH_WARN_SECS: i64 = 1800;

fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn durable_sync_auth_backoff_active() -> bool {
    unix_now_secs() < DURABLE_SYNC_AUTH_RETRY_AFTER.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record that a durable-queue flush found pending work but no valid auth.
/// Arms the retry backoff and emits a rate-limited warning.
pub(crate) fn note_durable_sync_unauthenticated(queue: &str, pending: i64) {
    let now = unix_now_secs();
    DURABLE_SYNC_AUTH_RETRY_AFTER.store(
        now + DURABLE_SYNC_AUTH_RETRY_SECS,
        std::sync::atomic::Ordering::Relaxed,
    );
    // Persist the blocked state so interactive commands can remind the user
    // (see auth::notice::maybe_warn_logged_out).
    crate::auth::notice::record_sync_auth_blocked();
    let last_warn = DURABLE_SYNC_LAST_AUTH_WARN.load(std::sync::atomic::Ordering::Relaxed);
    if now - last_warn >= DURABLE_SYNC_AUTH_WARN_SECS {
        DURABLE_SYNC_LAST_AUTH_WARN.store(now, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            queue,
            pending,
            "sync blocked: not authenticated; queued data will stay local until `autter login` succeeds"
        );
    }
}

/// Record a successful auth check so the backoff clears immediately.
pub(crate) fn note_durable_sync_authenticated() {
    DURABLE_SYNC_AUTH_RETRY_AFTER.store(0, std::sync::atomic::Ordering::Relaxed);
    crate::auth::notice::clear_sync_auth_blocked();
}

/// Record that a durable-queue upload failed after auth was available.
pub(crate) fn note_durable_sync_upload_failed(queue: &str, detail: &str) {
    crate::auth::notice::record_upload_failure(queue, detail);
}

fn flush_metrics(events: &[MetricEvent]) {
    // Local mode: metric events exist only to be uploaded, so drop them
    // instead of uploading or adding them to the durable retry queue.
    if !crate::config::platform_sync_enabled_now() {
        tracing::debug!(
            dropped = events.len(),
            "metrics: local mode; not uploading or queueing"
        );
        return;
    }
    if durable_sync_auth_backoff_active() {
        store_metrics_in_db(events);
        return;
    }
    // While the metrics endpoint is failing, new events join the durable
    // queue instead of trying (and timing out) again on every tick; the
    // durable replay retries them on its backoff schedule.
    let mut state = MetricsSyncState::load();
    let now = unix_now_secs();
    if state.clamp_backoff(now) {
        state.save();
    }
    if state.backoff_active(now) {
        store_metrics_in_db(events);
        return;
    }
    let context = ApiContext::new(None);
    let client = ApiClient::new(context);

    // Metrics are sent through the authenticated server-side API. Otherwise
    // the events fall back to the local SQLite queue.
    let should_upload = client.is_logged_in();

    let mut upload_failed = false;
    let mut state_changed = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

    // Envelopes are bounded by event count *and* serialized bytes: the server
    // rejects oversized bodies (HTTP 413), so a count-only chunk of large
    // events would stall the queue on every tick.
    for chunk in split_metrics_envelopes(events.to_vec()) {
        if should_upload && !upload_failed && std::time::Instant::now() < deadline {
            let batch = MetricsBatch::new(chunk.clone());
            match client.upload_metrics_classified(&batch) {
                Ok(response) if response.errors.is_empty() => {
                    note_durable_sync_authenticated();
                    // Older versions recorded metrics failures in the shared
                    // cloud-sync failure record; drop any left behind.
                    crate::auth::notice::clear_upload_failure_for("metrics");
                    state.record_success(unix_now_secs(), chunk.len());
                    state_changed = true;
                    continue;
                }
                Ok(response) => {
                    // The server answered; the durable replay sorts out which
                    // events it refused for good and which to retry.
                    tracing::warn!(
                        rejected = response.errors.len(),
                        "metrics: batch not fully accepted; queued for retry"
                    );
                    upload_failed = true;
                }
                Err(failure) => {
                    tracing::warn!(
                        endpoint = METRICS_UPLOAD_ENDPOINT,
                        kind = ?failure.kind(),
                        error = %failure.summary(),
                        events = chunk.len(),
                        "metrics: live upload failed; queued for retry"
                    );
                    state.record_failure(unix_now_secs(), &failure, chunk.len());
                    state_changed = true;
                    upload_failed = true;
                }
            }
        }
        store_metrics_in_db(&chunk);
    }
    if state_changed {
        state.save();
    }
}

/// True for per-event upload rejections that retrying can never fix: the
/// server validated the event content and refused it (HTTP 200 with an error
/// entry), so re-uploading identical bytes will fail identically. Such events
/// must be dropped loudly rather than re-queued forever — otherwise a few
/// stale oversized events keep the whole backlog (and the `upload_failing`
/// state) stuck. Any other per-event error keeps the current retry behavior.
///
/// Matches the base verdict and its reason-suffixed variants (the server is
/// rolling out machine-readable reasons such as
/// `"invalid metric event: database_insert_failed"`); the prefix form keeps
/// working against both the old opaque string and the new format.
fn is_validation_rejection(message: &str) -> bool {
    message == "invalid metric event" || message.starts_with("invalid metric event:")
}

/// Replay one bounded batch from the durable metrics queue. A bounded batch on
/// every three-second daemon tick drains backlogs steadily without monopolizing
/// the worker or delaying new checkpoints. After a failure the queue waits
/// (exponential backoff), and after a timeout or HTTP 413/504 it sends half as
/// many events next time, so a batch the server can't finish in time is never
/// replayed unchanged forever.
fn flush_stored_metrics() {
    // Local mode never uploads. A backlog left by an earlier connected session
    // stays queued (and visible in `autter doctor`) until the user reconnects
    // and chooses to import it, or purges it with `autter sync purge`.
    if !crate::config::platform_sync_enabled_now() {
        return;
    }
    let db = match MetricsDatabase::global() {
        Ok(db) => db,
        Err(error) => {
            tracing::warn!(%error, "metrics: failed to open durable queue");
            return;
        }
    };
    let pending = db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .count()
        .unwrap_or(0);
    if pending == 0 {
        return;
    }

    let now = unix_now_secs();
    let mut state = MetricsSyncState::load();
    // A clock that stepped backward must not leave the retry time far in the
    // future: the wait is bounded by the backoff, counted from now.
    if state.clamp_backoff(now) {
        state.save();
    }
    if state.backoff_active(now) {
        return;
    }

    let client = ApiClient::new(ApiContext::new(None));
    if !client.is_logged_in() && !client.has_api_key() {
        note_durable_sync_unauthenticated("metrics", pending as i64);
        if let Some(issue) = client.auth_issue_to_log() {
            tracing::warn!(reason = %issue, pending, "metrics: durable sync authentication unavailable");
        }
        return;
    }

    // The clock is read when each answer arrives, not before the request: a
    // request that takes a minute to fail must still be followed by the full
    // backoff.
    let outcome = replay_metrics_batch(db, &mut state, &unix_now_secs, &mut |batch| {
        client.upload_metrics_classified(batch)
    });
    if outcome.is_empty() {
        return;
    }
    state.save();

    let dropped = outcome.dropped_oversized + outcome.dropped_invalid;
    if outcome.failure.is_none() {
        note_durable_sync_authenticated();
        crate::auth::notice::clear_upload_failure_for("metrics");
    }
    tracing::info!(
        requested = outcome.requested,
        uploaded = outcome.accepted,
        rejected = outcome.retried,
        dropped_oversized = outcome.dropped_oversized,
        dropped_invalid = outcome.dropped_invalid,
        set_aside = outcome.set_aside,
        next_batch = state.batch_limit(),
        remaining = pending.saturating_sub(outcome.accepted + dropped + outcome.set_aside),
        "metrics: replayed durable queue batch"
    );
}

/// What one durable replay pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ReplayOutcome {
    /// Events asked of the queue (the current batch limit).
    pub requested: usize,
    pub accepted: usize,
    /// Events the server refused for a reason a retry may fix; re-queued.
    pub retried: usize,
    /// Single events too large for the endpoint (HTTP 413); dropped.
    pub dropped_oversized: usize,
    /// Events that can't be parsed or that the server refused to validate;
    /// dropped.
    pub dropped_invalid: usize,
    /// Single events the server failed on every time; moved out of the
    /// queue and kept.
    pub set_aside: usize,
    /// The failure that ended the pass, if one did.
    pub failure: Option<MetricsUploadFailure>,
}

impl ReplayOutcome {
    fn is_empty(&self) -> bool {
        self.accepted == 0
            && self.retried == 0
            && self.dropped_oversized == 0
            && self.dropped_invalid == 0
            && self.set_aside == 0
            && self.failure.is_none()
    }
}

/// Delete rows the server can never accept, and count them in `state` right
/// away: the count must not depend on how the rest of the pass goes.
fn drop_undeliverable(
    db: &std::sync::Mutex<MetricsDatabase>,
    state: &mut MetricsSyncState,
    ids: &[i64],
    why: &str,
) -> usize {
    if ids.is_empty() {
        return 0;
    }
    match db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .delete_records(ids)
    {
        Ok(()) => {
            state.record_dropped(ids.len());
            tracing::warn!(
                dropped = ids.len(),
                dropped_total = state.dropped_total,
                reason = why,
                "metrics: dropped events that can never upload"
            );
            ids.len()
        }
        Err(error) => {
            tracing::warn!(%error, reason = why, "metrics: failed to drop undeliverable events");
            0
        }
    }
}

/// Send up to `state.batch_limit()` queued events and update the queue and
/// `state` with the result. The first failed request ends the pass: the rest
/// of the batch waits for the backoff, at the smaller size if the failure
/// says a smaller batch could succeed.
///
/// `clock` returns unix seconds. It is read when an answer arrives, so the
/// backoff after a failure starts when the failure was seen, however long the
/// request took.
pub(crate) fn replay_metrics_batch(
    db: &std::sync::Mutex<MetricsDatabase>,
    state: &mut MetricsSyncState,
    clock: &dyn Fn() -> i64,
    upload: &mut dyn FnMut(&MetricsBatch) -> Result<MetricsUploadResponse, MetricsUploadFailure>,
) -> ReplayOutcome {
    let mut outcome = ReplayOutcome {
        requested: state.batch_limit(),
        ..ReplayOutcome::default()
    };
    let records = match db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_batch(outcome.requested)
    {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(%error, "metrics: failed to read durable queue");
            return outcome;
        }
    };

    let mut events = Vec::with_capacity(records.len());
    let mut uploaded_ids = Vec::with_capacity(records.len());
    let mut unparseable_ids = Vec::new();
    for record in records {
        match serde_json::from_str::<MetricEvent>(&record.event_json) {
            Ok(event) => {
                events.push(event);
                uploaded_ids.push(record.id);
            }
            Err(error) => {
                tracing::warn!(record_id = record.id, %error, "metrics: discarding invalid queued event");
                unparseable_ids.push(record.id);
            }
        }
    }

    outcome.dropped_invalid +=
        drop_undeliverable(db, state, &unparseable_ids, "not a metric event");
    if events.is_empty() {
        return outcome;
    }

    // Split into server-acceptable envelopes (bounded by count *and*
    // serialized bytes): one oversized envelope fails the whole batch with
    // HTTP 413 and the backlog never drains. Id slices track each envelope so
    // per-envelope results map back to the right rows; order is preserved.
    let envelopes = split_metrics_envelopes(events);
    let mut id_offset = 0;
    let mut accepted_ids: Vec<i64> = Vec::new();
    let mut retry_ids: Vec<i64> = Vec::new();
    let mut retry_errors: Vec<String> = Vec::new();
    let mut invalid_ids: Vec<i64> = Vec::new();
    // The server answered at least one request with a 200.
    let mut answered = false;
    // The largest request for which the server said "retry later" for some
    // rows: it ran out of time, so the next batch must be smaller than this.
    let mut out_of_time_size: Option<usize> = None;
    for envelope in envelopes {
        let ids = &uploaded_ids[id_offset..id_offset + envelope.len()];
        id_offset += envelope.len();
        let size = envelope.len();
        let batch = MetricsBatch::new(envelope);
        match upload(&batch) {
            Ok(response) => {
                answered = true;
                accepted_ids.extend(
                    response
                        .successful_indices(ids.len())
                        .into_iter()
                        .map(|index| ids[index]),
                );
                // Partition per-event rejections: validation verdicts on the
                // content itself (e.g. stale oversized events the server will
                // never accept) are dropped loudly; anything else is retried.
                let mut envelope_invalid = 0;
                for error in &response.errors {
                    let Some(id) = ids.get(error.index).copied() else {
                        continue;
                    };
                    if is_validation_rejection(&error.error) {
                        invalid_ids.push(id);
                        envelope_invalid += 1;
                    } else {
                        if crate::api::metrics::is_retry_later(&error.error) {
                            out_of_time_size = Some(out_of_time_size.unwrap_or(0).max(size));
                        }
                        retry_ids.push(id);
                        retry_errors.push(error.error.clone());
                    }
                }
                if envelope_invalid > 0 {
                    tracing::warn!(
                        rejected = envelope_invalid,
                        "metrics: dropping events the server refused to validate"
                    );
                }
            }
            Err(failure) => {
                let now = clock();
                // Poison message: a lone event the server rejects by size can
                // never upload through the size-limited endpoint. It is
                // deleted so it cannot wedge thousands of events behind it,
                // and the deletion is logged and counted.
                if ids.len() == 1 && failure.http_status() == Some(413) {
                    let bytes = serde_json::to_vec(&batch)
                        .map(|body| body.len())
                        .unwrap_or(0);
                    tracing::warn!(
                        record_id = ids[0],
                        request_bytes = bytes,
                        error = %failure.summary(),
                        "metrics: a single event is too large for the server (HTTP 413)"
                    );
                    outcome.dropped_oversized += drop_undeliverable(
                        db,
                        state,
                        ids,
                        "single event refused as too large (HTTP 413)",
                    );
                    continue;
                }
                tracing::warn!(
                    endpoint = METRICS_UPLOAD_ENDPOINT,
                    kind = ?failure.kind(),
                    error = %failure.summary(),
                    events = size,
                    "metrics: durable queue upload failed; retained for retry"
                );
                match state.record_batch_failure(now, &failure, size, ids[0]) {
                    FailedBatchAction::Retry => {
                        outcome.failure = Some(failure);
                        break;
                    }
                    FailedBatchAction::SetAside => {
                        // The same single event got the same error every
                        // time. It is kept, out of the queue, so the events
                        // behind it can upload.
                        let reason = failure.summary();
                        let moved = db
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .set_aside(ids, &reason, now);
                        match moved {
                            Ok(moved) if moved > 0 => {
                                state.record_set_aside(moved);
                                outcome.set_aside += moved;
                                tracing::warn!(
                                    record_id = ids[0],
                                    error = %reason,
                                    set_aside_total = state.set_aside_total,
                                    "metrics: set aside an event the server fails on every time; it is kept on this computer"
                                );
                                continue;
                            }
                            Ok(_) => {
                                outcome.failure = Some(failure);
                                break;
                            }
                            Err(error) => {
                                tracing::warn!(%error, "metrics: failed to set aside a failing event");
                                outcome.failure = Some(failure);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    if !accepted_ids.is_empty() || !retry_ids.is_empty() {
        let mut lock = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match lock.finish_upload(&accepted_ids, &retry_ids) {
            Ok(()) => {
                outcome.accepted = accepted_ids.len();
                outcome.retried = retry_ids.len();
            }
            Err(error) => {
                tracing::warn!(%error, "metrics: failed to update durable queue after upload");
            }
        }
    }
    // Events refused by validation are deleted so they can't wedge the queue
    // behind them.
    outcome.dropped_invalid += drop_undeliverable(
        db,
        state,
        &invalid_ids,
        "the server refused to validate them",
    );

    if outcome.failure.is_some() {
        // Requests before the one that failed may have uploaded events.
        state.uploaded_total += outcome.accepted as u64;
        return outcome;
    }
    let now = clock();
    if accepted_ids.is_empty() && !retry_ids.is_empty() {
        // The server answered but took none of the events, for a reason a
        // retry may fix. Wait before trying again. When the reason is that it
        // ran out of time, the failure also halves the batch.
        let failure = MetricsUploadFailure::NotAccepted {
            count: retry_ids.len(),
            error: retry_errors
                .iter()
                .find(|error| crate::api::metrics::is_retry_later(error))
                .or(retry_errors.first())
                .cloned()
                .unwrap_or_default(),
        };
        state.record_failure(now, &failure, out_of_time_size.unwrap_or(outcome.requested));
        state.repeated_failure = None;
        outcome.failure = Some(failure);
    } else if let Some(size) = out_of_time_size {
        // Part of the batch was written and the rest came back "retry
        // later": the server ran out of time, so send less next time.
        tracing::info!(
            events = size,
            accepted = outcome.accepted,
            retry_later = outcome.retried,
            "metrics: the server ran out of time on this batch; the next one is smaller"
        );
        state.record_partial_success(now, outcome.accepted, size);
    } else if answered || outcome.dropped_oversized > 0 {
        state.record_success(now, outcome.accepted);
    }
    outcome
}

fn store_metrics_in_db(events: &[MetricEvent]) {
    if events.is_empty() {
        return;
    }

    let event_jsons: Vec<String> = events
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect();

    if event_jsons.is_empty() {
        return;
    }

    if let Ok(db) = MetricsDatabase::global() {
        let mut db_lock = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = db_lock.insert_events(&event_jsons) {
            tracing::error!(%error, count = event_jsons.len(), "metrics: failed to persist retry queue");
        }
    }
}

fn flush_sentry_and_posthog(
    config: &Config,
    distinct_id: &str,
    errors: &[ErrorEvent],
    performances: &[PerformanceEvent],
    messages: &[MessageEvent],
) {
    // Check for Enterprise DSN
    let enterprise_dsn = config
        .telemetry_enterprise_dsn()
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::var("SENTRY_ENTERPRISE")
                .ok()
                .or_else(|| option_env!("SENTRY_ENTERPRISE").map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
        });

    // Check for OSS DSN
    let oss_dsn = if config.is_telemetry_oss_disabled() {
        None
    } else {
        std::env::var("SENTRY_OSS")
            .ok()
            .or_else(|| option_env!("SENTRY_OSS").map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
    };

    // PostHog destination for usage messages + error tracking. Resolves to
    // `None` (nothing sent or logged) when the user has not consented to
    // telemetry. Every event it captures is also mirrored to the local audit
    // log at ~/.autter/internal/telemetry.log.
    let posthog = crate::telemetry_client::PostHogClient::resolve(config);

    // Build Sentry clients
    let oss_client = oss_dsn.and_then(|dsn| SentryClient::from_dsn(&dsn));
    let enterprise_client = enterprise_dsn.and_then(|dsn| SentryClient::from_dsn(&dsn));

    // Build base tags
    let mut base_tags = BTreeMap::new();
    base_tags.insert("os".to_string(), json!(std::env::consts::OS));
    base_tags.insert("arch".to_string(), json!(std::env::consts::ARCH));
    base_tags.insert("distinct_id".to_string(), json!(distinct_id));

    // Send errors
    for error in errors {
        let mut extra = BTreeMap::new();
        if let Some(ctx) = &error.context
            && let Some(obj) = ctx.as_object()
        {
            for (key, value) in obj {
                extra.insert(key.clone(), value.clone());
            }
        }

        let event = json!({
            "message": error.message,
            "level": "error",
            "timestamp": error.timestamp,
            "platform": "other",
            "tags": base_tags,
            "extra": extra,
            "release": format!("autter@{}", env!("CARGO_PKG_VERSION")),
        });

        if let Some(client) = &oss_client {
            let _ = client.send_event(event.clone());
        }
        if let Some(client) = &enterprise_client {
            let _ = client.send_event(event);
        }

        // Error tracking via PostHog: emitted as a `$exception` event so it
        // lands in PostHog's Error Tracking product.
        if let Some(ph) = &posthog {
            // Prefer the real error variant (recorded as `error_kind`) so that
            // distinct kinds of failure group separately in error tracking
            // instead of all collapsing under the generic `AutterError` type.
            let exception_type = error
                .context
                .as_ref()
                .and_then(|ctx| ctx.get("error_kind"))
                .and_then(|v| v.as_str())
                .unwrap_or("AutterError");

            let mut props = BTreeMap::new();
            props.insert("$exception_message".to_string(), json!(error.message));
            props.insert(
                "$exception_list".to_string(),
                json!([{ "type": exception_type, "value": error.message }]),
            );
            props.insert("level".to_string(), json!("error"));
            if let Some(ctx) = &error.context
                && let Some(obj) = ctx.as_object()
            {
                for (key, value) in obj {
                    props.insert(key.clone(), value.clone());
                }
            }
            ph.capture(distinct_id, "$exception", props);
        }
    }

    // Send performance events
    for perf in performances {
        let mut extra = BTreeMap::new();
        extra.insert("operation".to_string(), json!(perf.operation));
        extra.insert("duration_ms".to_string(), json!(perf.duration_ms));
        if let Some(ctx) = &perf.context
            && let Some(obj) = ctx.as_object()
        {
            for (key, value) in obj {
                extra.insert(key.clone(), value.clone());
            }
        }

        let mut perf_tags = base_tags.clone();
        if let Some(tags) = &perf.tags {
            for (key, value) in tags {
                perf_tags.insert(key.clone(), json!(value));
            }
        }

        let event = json!({
            "message": format!("Performance: {} ({}ms)", perf.operation, perf.duration_ms),
            "level": "info",
            "timestamp": perf.timestamp,
            "platform": "other",
            "tags": perf_tags,
            "extra": extra,
            "release": format!("autter@{}", env!("CARGO_PKG_VERSION")),
        });

        if let Some(client) = &oss_client {
            let _ = client.send_event(event.clone());
        }
        if let Some(client) = &enterprise_client {
            let _ = client.send_event(event);
        }
    }

    // Send messages (to Sentry + PostHog)
    for msg in messages {
        let mut extra = BTreeMap::new();
        if let Some(ctx) = &msg.context
            && let Some(obj) = ctx.as_object()
        {
            for (key, value) in obj {
                extra.insert(key.clone(), value.clone());
            }
        }

        let sentry_event = json!({
            "message": msg.message,
            "level": msg.level,
            "timestamp": msg.timestamp,
            "platform": "other",
            "tags": base_tags,
            "extra": extra,
            "release": format!("autter@{}", env!("CARGO_PKG_VERSION")),
        });

        if let Some(client) = &oss_client {
            let _ = client.send_event(sentry_event.clone());
        }
        if let Some(client) = &enterprise_client {
            let _ = client.send_event(sentry_event);
        }

        // Usage tracking via PostHog: forward the message as a named event.
        // Device properties and the local audit-log mirror are handled inside
        // `capture`.
        if let Some(ph) = &posthog {
            let mut props = BTreeMap::new();
            props.insert("message".to_string(), json!(msg.message));
            props.insert("level".to_string(), json!(msg.level));
            if let Some(ctx) = &msg.context
                && let Some(obj) = ctx.as_object()
            {
                for (key, value) in obj {
                    props.insert(key.clone(), value.clone());
                }
            }
            ph.capture(distinct_id, &msg.message, props);
        }
    }
}

/// Flush pending notes from `notes-db` to the remote HTTP backend.
///
/// Skips silently when:
/// - the machine is in local mode (`Config::platform_sync_enabled`)
/// - `notes_backend.kind != Http`
/// - Not authenticated (no API key and not logged in)
pub fn flush_notes() {
    use crate::api::types::{NoteEntry, NotesUploadRequest};

    let cfg = Config::fresh();
    if !cfg.platform_sync_enabled() {
        tracing::debug!("notes: skipping flush, local mode never uploads");
        return;
    }
    if !cfg.notes_backend_kind().uses_http() {
        tracing::debug!("notes: skipping flush, backend does not use Http");
        return;
    }
    // `ControlRequest::FlushNotes` (sent after every note write) calls this
    // directly, outside the periodic loop's gate: never drain before the
    // pre-upgrade backlog is held (see crate::upload_hold).
    if let Err(error) = crate::upload_hold::ensure_legacy_migration() {
        tracing::warn!(%error, "notes: upload hold not applied; skipping flush");
        return;
    }

    let backend_url = match cfg.notes_backend_url() {
        Some(url) => url.to_string(),
        None => {
            tracing::debug!("notes: skipping flush, notes_backend.backend_url is not configured");
            return;
        }
    };

    // Cheap local check first: with nothing queued, skip building the API
    // client entirely (constructing it can trigger a token refresh).
    let notes_db = match crate::notes::db::NotesDatabase::global() {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(%e, "notes: failed to get notes DB");
            return;
        }
    };
    let pending_count = {
        let Ok(lock) = notes_db.lock() else {
            tracing::warn!("notes: DB lock poisoned");
            return;
        };
        lock.count_pending().unwrap_or(0)
    };
    if pending_count == 0 {
        return;
    }

    let context = ApiContext::new(Some(backend_url.clone()));
    let client = ApiClient::new(context);

    if !client.is_logged_in() && !client.has_api_key() {
        note_durable_sync_unauthenticated("notes", pending_count);
        if let Some(issue) = client.auth_issue_to_log() {
            tracing::warn!(reason = %issue, pending = pending_count, "notes: sync authentication unavailable");
        }
        return;
    }

    // Dequeue up to 50 pending notes.
    let pending = {
        let Ok(mut lock) = notes_db.lock() else {
            tracing::warn!("notes: DB lock poisoned");
            return;
        };
        match lock.dequeue_pending(50) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(%e, "notes: failed to dequeue pending rows");
                return;
            }
        }
    };

    if pending.is_empty() {
        return;
    }

    // Route each note to the org that connected its repo. A note whose repo no
    // org of the user's has connected (or that has no repo) stays on this
    // machine; it is never sent to the login org instead.
    type NoteBatchRow = (String, String, Option<String>);
    let mut groups: std::collections::HashMap<Option<String>, Vec<NoteBatchRow>> =
        std::collections::HashMap::new();
    for note in &pending {
        groups.entry(note.repo_url.clone()).or_default().push((
            note.commit_sha.clone(),
            note.content.clone(),
            note.repo_url.clone(),
        ));
    }

    for (repo_opt, batch) in groups {
        let commit_shas: Vec<String> = batch.iter().map(|(sha, _, _)| sha.clone()).collect();
        let entries: Vec<NoteEntry> = batch
            .iter()
            .map(|(sha, content, repo_url)| NoteEntry {
                commit_sha: sha.clone(),
                content: content.clone(),
                repo_url: repo_url.clone(),
            })
            .collect();

        let group_client = match crate::api::client::route_for_repo_at(
            repo_opt.as_deref(),
            Some(backend_url.clone()),
        ) {
            crate::api::client::RepoRoute::Upload(client) => client,
            crate::api::client::RepoRoute::NotConnected => {
                if let Ok(db) = crate::notes::db::NotesDatabase::global()
                    && let Ok(mut lock) = db.lock()
                {
                    let _ = lock.defer_unconnected(&commit_shas);
                }
                continue;
            }
            crate::api::client::RepoRoute::Unavailable(reason) => {
                if let Ok(db) = crate::notes::db::NotesDatabase::global()
                    && let Ok(mut lock) = db.lock()
                {
                    let _ = lock.mark_failed(&commit_shas, &reason);
                }
                continue;
            }
        };

        let request = NotesUploadRequest { entries };
        match group_client.upload_notes(request) {
            Ok(resp) => {
                // Notes the server refused because the repo isn't connected are
                // parked; only the rest count as uploaded or failed.
                let rejected: std::collections::HashSet<&str> = resp
                    .rejected
                    .iter()
                    .filter(|r| r.error == crate::api::client::REPO_NOT_CONNECTED)
                    .map(|r| r.commit_sha.as_str())
                    .collect();
                let (parked, rest): (Vec<String>, Vec<String>) = commit_shas
                    .iter()
                    .cloned()
                    .partition(|sha| rejected.contains(sha.as_str()));
                let other_failures = resp.failure_count.saturating_sub(parked.len());

                if resp.success_count > 0 {
                    note_durable_sync_authenticated();
                    if other_failures == 0 {
                        crate::auth::notice::clear_upload_failure_for("notes");
                    }
                }
                tracing::debug!(
                    success = resp.success_count,
                    failure = resp.failure_count,
                    not_connected = parked.len(),
                    "notes: uploaded batch"
                );
                if let Ok(db) = crate::notes::db::NotesDatabase::global()
                    && let Ok(mut lock) = db.lock()
                {
                    let _ = lock.defer_unconnected(&parked);
                    if other_failures == 0 {
                        let _ = lock.mark_synced(&rest);
                    } else {
                        // Server reported partial failures but doesn't identify which
                        // entries failed. Mark the rest of the group failed so they
                        // retry on the next flush cycle.
                        let reason = format!(
                            "partial failure: {}/{} entries failed",
                            other_failures,
                            rest.len()
                        );
                        note_durable_sync_upload_failed("notes", &reason);
                        let _ = lock.mark_failed(&rest, &reason);
                    }
                }
            }
            Err(e) => {
                note_durable_sync_upload_failed("notes", &e.to_string());
                tracing::warn!(%e, "notes: upload error");
                if let Ok(db) = crate::notes::db::NotesDatabase::global()
                    && let Ok(mut lock) = db.lock()
                {
                    let _ = lock.mark_failed(&commit_shas, &e.to_string());
                }
            }
        }
    }

    // Opportunistic cache eviction (~every 5 minutes at 3s flush interval).
    use std::sync::atomic::{AtomicU32, Ordering};
    static FLUSH_COUNT: AtomicU32 = AtomicU32::new(0);
    if FLUSH_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(100)
        && let Ok(db) = crate::notes::db::NotesDatabase::global()
        && let Ok(mut lock) = db.lock()
    {
        let _ = lock.evict_stale_cache(10_000, 90 * 24 * 3600);
    }
}

/// Build the CAS data-plane client and report whether uploads are currently
/// possible (i.e. we have credentials when targeting the hosted plane).
///
/// CAS (prompt transcripts) is data-plane traffic. When the HTTP notes backend
/// is active, send it to the same hosted data plane as notes (the API base,
/// via `Config::notes_backend_url()`); otherwise fall back to the API base
/// URL (legacy behavior).
fn cas_client() -> (ApiClient, bool) {
    let (client, enabled, _) = cas_target();
    (client, enabled)
}

/// [`cas_client`], plus the data-plane base URL when it is the hosted one
/// (`None` for a self-hosted plane, which has a single org and no routing).
fn cas_target() -> (ApiClient, bool, Option<Option<String>>) {
    let cfg = Config::fresh();
    let dataplane_url = if cfg.notes_backend_kind().uses_http() {
        cfg.notes_backend_url().map(|s| s.to_string())
    } else {
        None
    };
    let context = ApiContext::new(dataplane_url.clone());
    let target_url = context.base_url.clone();
    let client = ApiClient::new(context);

    let using_hosted = target_url == crate::config::DEFAULT_API_BASE_URL
        || target_url == crate::config::DEFAULT_NOTES_BACKEND_URL;
    let enabled = !using_hosted || client.is_logged_in() || client.has_api_key();
    (client, enabled, using_hosted.then_some(dataplane_url))
}

fn flush_cas(records: Vec<CasSyncPayload>) {
    // Local mode: transcripts never leave the machine.
    if !crate::config::platform_sync_enabled_now() {
        tracing::debug!(
            dropped = records.len(),
            "telemetry: local mode; not uploading CAS objects"
        );
        return;
    }
    let (client, enabled, hosted_base) = cas_target();
    if !enabled {
        tracing::debug!("telemetry: skipping CAS flush, not logged in");
        return;
    }

    // Build upload request
    let mut cas_objects = Vec::new();
    for record in &records {
        let content: Value = match serde_json::from_str(&record.data) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%e, "telemetry: CAS parse error");
                mark_cas_upload_failed(std::slice::from_ref(&record.hash), &e.to_string());
                continue;
            }
        };
        // Convert serialized JSON metadata string to HashMap
        let metadata = record
            .metadata
            .as_ref()
            .and_then(|m| serde_json::from_str::<std::collections::HashMap<String, String>>(m).ok())
            .unwrap_or_default();
        cas_objects.push(CasObject {
            content,
            hash: record.hash.clone(),
            metadata,
        });
    }

    if cas_objects.is_empty() {
        return;
    }

    // On the hosted plane, each transcript goes to the org that connected its
    // repo (from `metadata.repo_url`). Transcripts whose repo no org of the
    // user's has connected, or with no repo at all, stay on this machine.
    let Some(base_url) = hosted_base else {
        upload_cas_objects(&client, cas_objects);
        return;
    };
    let mut by_repo: std::collections::HashMap<Option<String>, Vec<CasObject>> =
        std::collections::HashMap::new();
    for object in cas_objects {
        by_repo
            .entry(object.metadata.get("repo_url").cloned())
            .or_default()
            .push(object);
    }
    for (repo_url, objects) in by_repo {
        match crate::api::client::route_for_repo_at(repo_url.as_deref(), base_url.clone()) {
            crate::api::client::RepoRoute::Upload(repo_client) => {
                upload_cas_objects(&repo_client, objects);
            }
            crate::api::client::RepoRoute::NotConnected => {
                let hashes: Vec<String> = objects.iter().map(|o| o.hash.clone()).collect();
                defer_cas_unconnected(&hashes);
            }
            crate::api::client::RepoRoute::Unavailable(reason) => {
                let hashes: Vec<String> = objects.iter().map(|o| o.hash.clone()).collect();
                mark_cas_upload_failed(&hashes, &reason);
            }
        }
    }
}

fn defer_cas_unconnected(hashes: &[String]) {
    if let Ok(db) = crate::authorship::internal_db::InternalDatabase::global() {
        let mut lock = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = lock.defer_cas_unconnected(hashes);
    }
}

fn upload_cas_objects(client: &ApiClient, cas_objects: Vec<CasObject>) {
    for chunk in cas_objects.chunks(50) {
        let hashes: Vec<String> = chunk.iter().map(|o| o.hash.clone()).collect();
        let request = CasUploadRequest {
            objects: chunk.to_vec(),
        };
        match client.upload_cas(request) {
            Ok(response) => {
                let successful_hashes: Vec<String> = response
                    .results
                    .iter()
                    .filter(|result| result.status == "ok")
                    .map(|result| result.hash.clone())
                    .collect();
                // Delete successfully uploaded records from the internal DB queue
                // so they don't accumulate as stale entries.
                if !successful_hashes.is_empty()
                    && let Ok(db) = crate::authorship::internal_db::InternalDatabase::global()
                    && let Ok(mut db_lock) = db.lock()
                {
                    let _ = db_lock.delete_cas_by_hashes(&successful_hashes);
                    note_durable_sync_authenticated();
                    crate::auth::notice::clear_upload_failure_for("transcripts");
                }
                // Refused because the repo isn't connected: park, don't retry.
                let mut parked_hashes = Vec::new();
                let mut failed_hashes = Vec::new();
                for result in response.results.iter().filter(|r| r.status != "ok") {
                    if result.error.as_deref() == Some(crate::api::client::REPO_NOT_CONNECTED) {
                        parked_hashes.push(result.hash.clone());
                    } else {
                        failed_hashes.push(result.hash.clone());
                    }
                }
                defer_cas_unconnected(&parked_hashes);
                if !failed_hashes.is_empty() {
                    mark_cas_upload_failed(&failed_hashes, "org database rejected CAS object");
                }
                tracing::debug!(
                    uploaded = successful_hashes.len(),
                    failed = hashes.len().saturating_sub(successful_hashes.len()),
                    "telemetry: uploaded CAS objects"
                );
            }
            Err(e) => {
                mark_cas_upload_failed(&hashes, &e.to_string());
                tracing::warn!(%e, "telemetry: CAS upload error");
            }
        }
    }
}

fn mark_cas_upload_failed(hashes: &[String], error: &str) {
    note_durable_sync_upload_failed("transcripts", error);
    if let Ok(db) = crate::authorship::internal_db::InternalDatabase::global() {
        let mut lock = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = lock.mark_cas_failed(hashes, error, DURABLE_SYNC_AUTH_RETRY_SECS);
    }
}

/// Drain the durable CAS queue (`cas_sync_queue` in the internal DB) and upload
/// in batches via [`flush_cas`], which deletes each record on success. Records
/// that fail to upload stay locked as `processing` and are recovered to
/// `pending` by `dequeue_cas_batch`'s stale-lock sweep on a later tick.
fn flush_cas_queue() {
    // Local mode never uploads transcripts, including a backlog queued by an
    // earlier connected session.
    if !crate::config::platform_sync_enabled_now() {
        return;
    }
    let Ok(db) = crate::authorship::internal_db::InternalDatabase::global() else {
        return;
    };

    // Cheap local check first: with nothing queued, skip building the API
    // client entirely (constructing it can trigger a token refresh).
    let pending = {
        let Ok(db_lock) = db.lock() else {
            return;
        };
        db_lock.count_pending_cas().unwrap_or(0)
    };
    if pending == 0 {
        return;
    }

    // Don't lock records as `processing` if we can't upload them anyway — that
    // would just churn the queue through the 10-minute stale-lock recovery.
    let (auth_client, enabled) = cas_client();
    if !enabled {
        note_durable_sync_unauthenticated("cas_transcripts", pending);
        if let Some(issue) = auth_client.auth_issue_to_log() {
            tracing::warn!(reason = %issue, pending, "cas: sync authentication unavailable");
        }
        return;
    }

    // Bound the number of batches per tick so a large backlog can't monopolize
    // the flush loop; the remainder is picked up on subsequent ticks.
    const MAX_BATCHES_PER_TICK: usize = 20;
    const BATCH_SIZE: usize = 50;

    for _ in 0..MAX_BATCHES_PER_TICK {
        let records = {
            let Ok(mut db_lock) = db.lock() else {
                return;
            };
            match db_lock.dequeue_cas_batch(BATCH_SIZE) {
                Ok(records) => records,
                Err(e) => {
                    tracing::warn!(%e, "telemetry: CAS dequeue error");
                    return;
                }
            }
        };

        if records.is_empty() {
            break;
        }

        let payloads: Vec<CasSyncPayload> = records
            .into_iter()
            .map(|record| CasSyncPayload {
                hash: record.hash,
                data: record.data,
                metadata: serde_json::to_string(&record.metadata).ok(),
            })
            .collect();

        flush_cas(payloads);
    }
}

/// Minimal Sentry client (mirrors flush.rs SentryClient)
struct SentryClient {
    endpoint: String,
    public_key: String,
}

impl SentryClient {
    fn from_dsn(dsn: &str) -> Option<Self> {
        let url = url::Url::parse(dsn).ok()?;
        let public_key = url.username().to_string();
        let host = url.host_str()?;
        let project_id = url.path().trim_start_matches('/');
        let scheme = url.scheme();
        let endpoint = format!("{}://{}/api/{}/store/", scheme, host, project_id);
        Some(SentryClient {
            endpoint,
            public_key,
        })
    }

    fn send_event(&self, event: Value) -> Result<(), Box<dyn std::error::Error>> {
        let auth_header = format!(
            "Sentry sentry_version=7, sentry_key={}, sentry_client=autter/{}",
            self.public_key,
            env!("CARGO_PKG_VERSION")
        );

        let body = serde_json::to_string(&event)?;
        let agent = crate::http::build_agent(Some(30));
        let request = agent
            .post(&self.endpoint)
            .set("X-Sentry-Auth", &auth_header)
            .set("Content-Type", "application/json");
        let response = crate::http::send_with_body(request, &body)?;

        let status = response.status_code;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("Sentry returned status {}", status).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_validation_rejection;
    use super::panic_message;

    #[test]
    fn panic_message_reads_str_and_string_payloads() {
        let str_panic = std::panic::catch_unwind(|| panic!("boom")).unwrap_err();
        assert_eq!(panic_message(&str_panic), "boom");

        let string_panic = std::panic::catch_unwind(|| panic!("count is {}", 3)).unwrap_err();
        assert_eq!(panic_message(&string_panic), "count is 3");
    }

    #[test]
    fn validation_rejection_matches_only_permanent_verdicts() {
        assert!(is_validation_rejection("invalid metric event"));
        assert!(is_validation_rejection(
            "invalid metric event: database_insert_failed"
        ));
        assert!(is_validation_rejection(
            "invalid metric event: nul byte in values"
        ));
        assert!(!is_validation_rejection(""));
        assert!(!is_validation_rejection("invalid metric event "));
        assert!(!is_validation_rejection("rate limited, retry later"));
        assert!(!is_validation_rejection("organization quota exceeded"));
        assert!(!is_validation_rejection("database_insert_failed"));
    }

    mod durable_metrics_replay {
        use super::super::{ReplayOutcome, replay_metrics_batch};
        use crate::api::metrics::{
            MetricsFailureKind, MetricsUploadError, MetricsUploadFailure, MetricsUploadResponse,
        };
        use crate::api::{ApiClient, ApiContext};
        use crate::metrics::MetricEvent;
        use crate::metrics::db::MetricsDatabase;
        use crate::metrics::sync_state::{MetricsSyncState, UsageMetricsStatus, describe_failure};
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        fn queue_with(events: usize) -> (Mutex<MetricsDatabase>, tempfile::TempDir) {
            let dir = tempfile::tempdir().unwrap();
            let mut db = MetricsDatabase::open_at(&dir.path().join("metrics.db")).unwrap();
            let rows: Vec<String> = (0..events)
                .map(|index| {
                    serde_json::to_string(&MetricEvent {
                        timestamp: 1_760_000_000 + index as u32,
                        event_id: 1,
                        values: Default::default(),
                        attrs: Default::default(),
                    })
                    .unwrap()
                })
                .collect();
            db.insert_events(&rows).unwrap();
            (Mutex::new(db), dir)
        }

        fn queued(db: &Mutex<MetricsDatabase>) -> usize {
            db.lock().unwrap().count().unwrap()
        }

        /// A metrics endpoint that takes longer than the client waits for
        /// any batch over `max_fast` events, and accepts smaller ones.
        struct SlowForLargeBatches {
            base_url: String,
            sizes: Arc<Mutex<Vec<usize>>>,
            stop: Arc<AtomicBool>,
        }

        impl SlowForLargeBatches {
            fn start(max_fast: usize, stall: Duration) -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                listener.set_nonblocking(true).unwrap();
                let base_url = format!("http://{}", listener.local_addr().unwrap());
                let sizes = Arc::new(Mutex::new(Vec::new()));
                let stop = Arc::new(AtomicBool::new(false));
                let (thread_sizes, thread_stop) = (Arc::clone(&sizes), Arc::clone(&stop));
                std::thread::spawn(move || {
                    while !thread_stop.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let sizes = Arc::clone(&thread_sizes);
                                std::thread::spawn(move || serve(stream, max_fast, stall, &sizes));
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(5)),
                        }
                    }
                });
                Self {
                    base_url,
                    sizes,
                    stop,
                }
            }
        }

        impl Drop for SlowForLargeBatches {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
            }
        }

        fn serve(
            mut stream: TcpStream,
            max_fast: usize,
            stall: Duration,
            sizes: &Mutex<Vec<usize>>,
        ) {
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            let header_end = loop {
                let Ok(read) = stream.read(&mut chunk) else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            while buffer.len() - header_end < length {
                let Ok(read) = stream.read(&mut chunk) else {
                    return;
                };
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&buffer[header_end..header_end + length]).unwrap();
            let size = body["events"].as_array().map_or(0, Vec::len);
            sizes.lock().unwrap().push(size);
            if size > max_fast {
                std::thread::sleep(stall);
            }
            let reply = r#"{"errors":[]}"#;
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            );
        }

        /// The reported bug: 643 queued events, a server too slow for the
        /// full batch. Before, every replay resent the same batch and timed
        /// out forever. Now each timeout halves the batch and the queue
        /// drains in smaller batches.
        #[test]
        fn a_queue_the_server_is_too_slow_for_drains_in_smaller_batches() {
            let server = SlowForLargeBatches::start(100, Duration::from_secs(3));
            let client = ApiClient::new(
                ApiContext::without_auth(Some(server.base_url.clone())).with_timeout(1),
            );
            let (db, _dir) = queue_with(643);
            let mut state = MetricsSyncState::default();
            let mut now = 1_000;
            let mut first_timeout: Option<(MetricsUploadFailure, MetricsSyncState)> = None;

            for _ in 0..40 {
                if queued(&db) == 0 {
                    break;
                }
                now = now.max(state.next_attempt_at);
                let outcome = replay_metrics_batch(&db, &mut state, &|| now, &mut |batch| {
                    client.upload_metrics_classified(batch)
                });
                if let Some(failure) = &outcome.failure
                    && first_timeout.is_none()
                {
                    first_timeout = Some((failure.clone(), state.clone()));
                }
            }

            assert_eq!(queued(&db), 0, "queue drained");
            assert!(state.last_failure.is_none(), "drained queue is healthy");
            assert_eq!(state.uploaded_total, 643);

            let sizes = server.sizes.lock().unwrap().clone();
            assert_eq!(
                &sizes[..4],
                &[643, 321, 160, 80],
                "each timeout halves the batch: {sizes:?}"
            );
            assert!(
                sizes.iter().filter(|size| **size <= 100).count() >= 5,
                "small batches are accepted: {sizes:?}"
            );

            // While it was failing, doctor/sync status name the endpoint and
            // the real error, not "produced no accepted batch".
            let (failure, failing_state) = first_timeout.expect("the full batch timed out");
            assert_eq!(failure.kind(), MetricsFailureKind::Timeout);
            let status = UsageMetricsStatus::from_state(&failing_state, 643, 1_000);
            assert!(status.failing);
            let described = describe_failure(status.last_failure.as_ref().unwrap());
            assert!(
                described.starts_with("POST /worker/metrics/upload: timeout after 1s"),
                "{described}"
            );
            assert!(described.contains("batch of 643 events"), "{described}");
            assert_eq!(status.batch_size, 321);
        }

        #[test]
        fn a_failure_ends_the_pass_and_backs_off_without_losing_events() {
            let (db, _dir) = queue_with(30);
            let mut state = MetricsSyncState::default();
            let outcome = replay_metrics_batch(&db, &mut state, &|| 50, &mut |_| {
                Err(MetricsUploadFailure::Http {
                    status: 500,
                    body: r#"{"error":"Data-plane operation failed"}"#.to_string(),
                })
            });
            assert_eq!(queued(&db), 30, "nothing dropped");
            assert_eq!(state.batch_limit(), 1000, "a server error does not split");
            assert!(state.backoff_active(50));
            let failure = state.last_failure.as_ref().unwrap();
            assert_eq!(failure.http_status, Some(500));
            assert_eq!(
                failure.error,
                r#"HTTP 500: {"error":"Data-plane operation failed"}"#
            );
            assert_eq!(outcome.accepted, 0);
        }

        #[test]
        fn a_lone_event_refused_as_too_large_is_dropped() {
            let (db, _dir) = queue_with(1);
            let mut state = MetricsSyncState::default();
            let outcome = replay_metrics_batch(&db, &mut state, &|| 0, &mut |_| {
                Err(MetricsUploadFailure::Http {
                    status: 413,
                    body: String::new(),
                })
            });
            assert_eq!(queued(&db), 0);
            assert_eq!(
                outcome,
                ReplayOutcome {
                    requested: 1000,
                    dropped_oversized: 1,
                    ..ReplayOutcome::default()
                }
            );
        }

        fn set_aside(db: &Mutex<MetricsDatabase>) -> usize {
            db.lock().unwrap().count_set_aside().unwrap()
        }

        fn http(status: u16) -> MetricsUploadFailure {
            MetricsUploadFailure::Http {
                status,
                body: r#"{"error":"Data-plane operation failed"}"#.to_string(),
            }
        }

        fn ok() -> Result<MetricsUploadResponse, MetricsUploadFailure> {
            Ok(MetricsUploadResponse { errors: Vec::new() })
        }

        /// A request that takes a minute to fail. The backoff used to be
        /// counted from before the request, so the 5-second wait was already
        /// over when the failure arrived and the retry fired on the next
        /// tick.
        #[test]
        fn the_backoff_starts_when_the_failure_is_seen_not_when_the_request_started() {
            let (db, _dir) = queue_with(10);
            let mut state = MetricsSyncState::default();
            let clock = std::cell::Cell::new(1_000_i64);
            let outcome = replay_metrics_batch(&db, &mut state, &|| clock.get(), &mut |_| {
                clock.set(clock.get() + 60);
                Err(MetricsUploadFailure::Timeout {
                    after_secs: Some(60),
                })
            });
            assert!(outcome.failure.is_some());
            assert_eq!(state.last_failure.as_ref().unwrap().at, 1_060);
            assert_eq!(state.next_attempt_at, 1_065);
            assert!(
                state.backoff_active(clock.get()),
                "the queue still waits after a slow failure"
            );
        }

        /// Two events that each fill a request on their own: the first is
        /// refused as too large and dropped, the second fails. The drop used
        /// to be counted only when the whole pass succeeded.
        #[test]
        fn a_dropped_oversized_event_is_counted_even_when_the_pass_then_fails() {
            let dir = tempfile::tempdir().unwrap();
            let mut queue = MetricsDatabase::open_at(&dir.path().join("metrics.db")).unwrap();
            let big = |index: u32| {
                let mut attrs = std::collections::HashMap::new();
                attrs.insert(
                    "0".to_string(),
                    serde_json::Value::String("x".repeat(2_500_000)),
                );
                serde_json::to_string(&MetricEvent {
                    timestamp: 1_760_000_000 + index,
                    event_id: 1,
                    values: Default::default(),
                    attrs,
                })
                .unwrap()
            };
            queue.insert_events(&[big(0), big(1)]).unwrap();
            let db = Mutex::new(queue);
            let mut state = MetricsSyncState::default();
            let mut calls = 0;
            let outcome = replay_metrics_batch(&db, &mut state, &|| 100, &mut |batch| {
                calls += 1;
                assert_eq!(batch.events.len(), 1, "each event is its own request");
                Err(if calls == 1 { http(413) } else { http(503) })
            });
            assert_eq!(calls, 2);
            assert_eq!(outcome.dropped_oversized, 1);
            assert_eq!(queued(&db), 1, "the second event is kept for retry");
            assert_eq!(set_aside(&db), 0);
            assert_eq!(state.dropped_total, 1, "the drop is counted");
            assert_eq!(state.last_failure.as_ref().unwrap().http_status, Some(503));
            let status = UsageMetricsStatus::from_state(&state, 1, 100);
            assert!(
                status.summary_line().contains("1 dropped as unacceptable"),
                "{}",
                status.summary_line()
            );
        }

        #[test]
        fn a_queued_row_that_is_not_an_event_is_counted_when_dropped() {
            let dir = tempfile::tempdir().unwrap();
            let mut queue = MetricsDatabase::open_at(&dir.path().join("metrics.db")).unwrap();
            queue.insert_events(&["not json".to_string()]).unwrap();
            let db = Mutex::new(queue);
            let mut state = MetricsSyncState::default();
            let outcome =
                replay_metrics_batch(&db, &mut state, &|| 0, &mut |_| panic!("nothing to upload"));
            assert_eq!(outcome.dropped_invalid, 1);
            assert_eq!(queued(&db), 0);
            assert_eq!(state.dropped_total, 1);
        }

        /// The server wrote the first 400 events and answered "retry later"
        /// for the rest (its 20-second write budget ran out). The same full
        /// batch used to be sent again.
        #[test]
        fn retry_later_for_part_of_a_batch_halves_the_next_one() {
            let (db, _dir) = queue_with(1000);
            let mut state = MetricsSyncState::default();
            let mut sizes = Vec::new();
            let outcome = replay_metrics_batch(&db, &mut state, &|| 100, &mut |batch| {
                sizes.push(batch.events.len());
                Ok(MetricsUploadResponse {
                    errors: (400..batch.events.len())
                        .map(|index| MetricsUploadError {
                            index,
                            error: "retry later: the server ran out of time writing this batch"
                                .to_string(),
                        })
                        .collect(),
                })
            });
            assert_eq!(outcome.accepted, 400);
            assert_eq!(outcome.retried, 600);
            assert!(outcome.failure.is_none(), "progress is not a failure");
            assert_eq!(queued(&db), 600, "the rest is kept");
            assert_eq!(state.batch_limit(), 500, "the next batch is half the size");
            assert!(!state.backoff_active(100));
            assert_eq!(state.uploaded_total, 400);

            // The next pass sends 500 events, not 1,000.
            replay_metrics_batch(&db, &mut state, &|| 101, &mut |batch| {
                sizes.push(batch.events.len());
                ok()
            });
            assert_eq!(sizes, vec![1000, 500]);
            assert_eq!(queued(&db), 100);
        }

        #[test]
        fn retry_later_for_a_whole_batch_halves_it_and_backs_off() {
            let (db, _dir) = queue_with(1000);
            let mut state = MetricsSyncState::default();
            let outcome = replay_metrics_batch(&db, &mut state, &|| 100, &mut |batch| {
                Ok(MetricsUploadResponse {
                    errors: (0..batch.events.len())
                        .map(|index| MetricsUploadError {
                            index,
                            error: "retry later: the server ran out of time writing this batch"
                                .to_string(),
                        })
                        .collect(),
                })
            });
            assert_eq!(queued(&db), 1000, "nothing dropped");
            assert_eq!(state.batch_limit(), 500);
            assert!(state.backoff_active(100));
            let failure = outcome.failure.expect("nothing was accepted");
            assert_eq!(failure.kind(), MetricsFailureKind::TooLarge);
            let record = state.last_failure.as_ref().unwrap();
            assert_eq!(record.batch_size, 1000);
            assert!(record.error.contains("retry later"), "{}", record.error);
        }

        /// Drive the queue until it is empty or `passes` run out, moving the
        /// clock to each retry time. The server fails with HTTP 500 for any
        /// request that contains an event `is_bad` says is bad.
        fn drain(
            db: &Mutex<MetricsDatabase>,
            state: &mut MetricsSyncState,
            passes: usize,
            is_bad: &dyn Fn(&MetricEvent) -> bool,
        ) -> (Vec<usize>, i64) {
            let mut sizes = Vec::new();
            let mut now = 1_000;
            for _ in 0..passes {
                if queued(db) == 0 {
                    break;
                }
                now = now.max(state.next_attempt_at);
                replay_metrics_batch(db, state, &|| now, &mut |batch| {
                    sizes.push(batch.events.len());
                    if batch.events.iter().any(is_bad) {
                        Err(http(500))
                    } else {
                        ok()
                    }
                });
            }
            (sizes, now - 1_000)
        }

        /// The reported bug: the server answers HTTP 500 for the same rows
        /// every time. The first 1,000 rows were re-read and re-sent forever,
        /// so nothing behind them uploaded. Now the batch is narrowed down to
        /// the failing rows, they are set aside (kept), and the rest drains.
        #[test]
        fn a_server_error_tied_to_two_events_sets_them_aside_and_the_rest_drains() {
            let (db, _dir) = queue_with(1200);
            let bad = [1_760_000_000 + 3, 1_760_000_000 + 700];
            let mut state = MetricsSyncState::default();
            let (sizes, elapsed) = drain(&db, &mut state, 400, &|event| {
                bad.contains(&event.timestamp)
            });

            assert_eq!(queued(&db), 0, "the queue drained past the failing rows");
            assert_eq!(set_aside(&db), 2, "the two failing events are kept");
            assert_eq!(state.set_aside_total, 2);
            assert_eq!(state.uploaded_total, 1198);
            assert_eq!(state.dropped_total, 0, "nothing was deleted");
            assert_eq!(
                &sizes[..5],
                &[1000, 1000, 1000, 500, 250],
                "three identical failures, then the batch is halved: {sizes:?}"
            );
            assert!(elapsed < 3600, "isolation is bounded: took {elapsed}s");
            assert_eq!(
                db.lock()
                    .unwrap()
                    .last_set_aside_reason()
                    .unwrap()
                    .as_deref(),
                Some(r#"HTTP 500: {"error":"Data-plane operation failed"}"#)
            );

            // doctor and sync status say how many, and what to do.
            let status = UsageMetricsStatus::from_state(&state, 0, 0).with_set_aside(2, None);
            assert!(!status.failing);
            assert!(status.summary_line().contains("2 set aside"));

            // `autter sync retry` queues them again; with the server fixed
            // they upload.
            assert_eq!(db.lock().unwrap().release_set_aside().unwrap(), 2);
            assert_eq!(queued(&db), 2);
            drain(&db, &mut state, 5, &|_| false);
            assert_eq!(queued(&db), 0);
            assert_eq!(state.uploaded_total, 1200);
        }

        /// When the server fails on everything (an outage), events are still
        /// never deleted, and few are set aside: the wait between attempts
        /// keeps growing.
        #[test]
        fn a_server_that_fails_on_everything_loses_nothing() {
            let (db, _dir) = queue_with(50);
            let mut state = MetricsSyncState::default();
            let mut now = 1_000;
            while now < 1_000 + 2 * 3600 {
                now = now.max(state.next_attempt_at) + 3;
                replay_metrics_batch(&db, &mut state, &|| now, &mut |_| Err(http(500)));
            }
            assert_eq!(queued(&db) + set_aside(&db), 50, "every event is kept");
            assert_eq!(state.dropped_total, 0);
            assert!(
                set_aside(&db) <= 4,
                "two hours of failures set aside {} events",
                set_aside(&db)
            );
        }

        #[test]
        fn retryable_per_event_errors_with_nothing_accepted_back_off() {
            let (db, _dir) = queue_with(3);
            let mut state = MetricsSyncState::default();
            let outcome = replay_metrics_batch(&db, &mut state, &|| 0, &mut |batch| {
                Ok(MetricsUploadResponse {
                    errors: (0..batch.events.len())
                        .map(|index| MetricsUploadError {
                            index,
                            error: "rate limited".to_string(),
                        })
                        .collect(),
                })
            });
            assert_eq!(queued(&db), 3, "re-queued, not dropped");
            assert_eq!(outcome.retried, 3);
            assert!(state.backoff_active(0));
            assert_eq!(
                state.last_failure.as_ref().unwrap().error,
                "server accepted none of 3 events: rate limited"
            );
        }
    }
}
