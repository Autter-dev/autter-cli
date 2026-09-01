//! Daemon-side telemetry worker that batches and dispatches events.
//!
//! Runs inside the daemon process using tokio. Accumulates telemetry envelopes
//! and CAS payloads, then flushes them to their destinations every 3 seconds.

use crate::api::{ApiClient, ApiContext, CasObject, CasUploadRequest};
use crate::config::{Config, get_or_create_distinct_id};
use crate::daemon::control_api::{CasSyncPayload, TelemetryEnvelope};
use crate::metrics::db::MetricsDatabase;
use crate::metrics::{MetricEvent, MetricsBatch};
use crate::observability::MAX_METRICS_PER_ENVELOPE;
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
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
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

    // Flush metrics (always processed — uploaded or stored in SQLite)
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
    // expired login doesn't trigger a token-refresh network call on every tick.
    if !durable_sync_auth_backoff_active() {
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
    use crate::api::org_db::{self, CommitAuthorshipSummaryRow};

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
        if let Some(issue) = home_client.auth_issue() {
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
        let row = match serde_json::from_str::<CommitAuthorshipSummaryRow>(&pending_row.payload) {
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

        let client = if let Some(org) = row
            .repo_url
            .as_deref()
            .and_then(crate::api::client::resolve_org_for_repo_cached)
        {
            match crate::api::client::access_token_for_org(&org) {
                Some(token) => ApiClient::new(ApiContext::with_auth(None, token)),
                None => {
                    let mut lock = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    let _ = lock.mark_commit_summary_failed(
                        &pending_row.commit_sha,
                        "could not mint org-scoped token",
                    );
                    continue;
                }
            }
        } else {
            ApiClient::new(ApiContext::new(None))
        };

        let result = client.org_identity().and_then(|identity| {
            org_db::upsert_commit_authorship_summary(&identity, &row, &get_or_create_distinct_id())
        });
        let mut lock = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match result {
            Ok(()) => {
                if let Err(error) = lock.mark_commit_summary_synced(&pending_row.commit_sha) {
                    tracing::warn!(commit_sha = %pending_row.commit_sha, %error, "commit summaries: uploaded row remains queued");
                } else {
                    note_durable_sync_authenticated();
                }
            }
            Err(error) => {
                let _ =
                    lock.mark_commit_summary_failed(&pending_row.commit_sha, &error.to_string());
                note_durable_sync_upload_failed();
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
pub(crate) fn note_durable_sync_upload_failed() {
    crate::auth::notice::record_sync_upload_stalled();
}

fn flush_metrics(events: &[MetricEvent]) {
    if durable_sync_auth_backoff_active() {
        store_metrics_in_db(events);
        return;
    }
    let context = ApiContext::new(None);
    let client = ApiClient::new(context);

    // Metrics are written straight to the org database, which we reach via the
    // `org_db_url` claim in the access token — so a write is only possible when
    // logged in. Otherwise the events fall back to the local SQLite queue.
    let should_upload = client.is_logged_in();

    let mut upload_failed = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

    for chunk in events.chunks(MAX_METRICS_PER_ENVELOPE) {
        if should_upload && !upload_failed && std::time::Instant::now() < deadline {
            let batch = MetricsBatch::new(chunk.to_vec());
            match client.upload_metrics(&batch) {
                Ok(_) => {
                    note_durable_sync_authenticated();
                    continue;
                }
                Err(error) => {
                    tracing::warn!(%error, "metrics: live upload failed; queued for retry");
                    note_durable_sync_upload_failed();
                    upload_failed = true;
                }
            }
        }
        store_metrics_in_db(chunk);
    }
}

/// Replay one bounded batch from the durable metrics queue. A bounded batch on
/// every three-second daemon tick drains backlogs steadily without monopolizing
/// the worker or delaying new checkpoints.
fn flush_stored_metrics() {
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

    let client = ApiClient::new(ApiContext::new(None));
    if !client.is_logged_in() && !client.has_api_key() {
        note_durable_sync_unauthenticated("metrics", pending as i64);
        if let Some(issue) = client.auth_issue() {
            tracing::warn!(reason = %issue, pending, "metrics: durable sync authentication unavailable");
        }
        return;
    }

    let records = match db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_batch(MAX_METRICS_PER_ENVELOPE)
    {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(%error, "metrics: failed to read durable queue");
            return;
        }
    };

    let mut events = Vec::with_capacity(records.len());
    let mut uploaded_ids = Vec::with_capacity(records.len());
    let mut invalid_ids = Vec::new();
    for record in records {
        match serde_json::from_str::<MetricEvent>(&record.event_json) {
            Ok(event) => {
                events.push(event);
                uploaded_ids.push(record.id);
            }
            Err(error) => {
                tracing::warn!(record_id = record.id, %error, "metrics: discarding invalid queued event");
                invalid_ids.push(record.id);
            }
        }
    }

    if !invalid_ids.is_empty() {
        let _ = db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .delete_records(&invalid_ids);
    }
    if events.is_empty() {
        return;
    }

    match client.upload_metrics(&MetricsBatch::new(events)) {
        Ok(response) => {
            // Per-row errors are validation failures and cannot become valid on
            // retry. They are already reported by upload_metrics; removing the
            // whole idempotent batch prevents one malformed event from blocking
            // the queue forever.
            let _ = db
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .delete_records(&uploaded_ids);
            note_durable_sync_authenticated();
            tracing::info!(
                uploaded = uploaded_ids.len(),
                rejected = response.errors.len(),
                remaining = pending.saturating_sub(uploaded_ids.len()),
                "metrics: replayed durable queue batch"
            );
        }
        Err(error) => {
            note_durable_sync_upload_failed();
            tracing::warn!(%error, pending, "metrics: durable queue upload failed; retained for retry");
        }
    }
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
/// - `notes_backend.kind != Http`
/// - Not authenticated (no API key and not logged in)
pub fn flush_notes() {
    use crate::api::types::{NoteEntry, NotesUploadRequest};

    let cfg = Config::fresh();
    if !cfg.notes_backend_kind().uses_http() {
        tracing::debug!("notes: skipping flush, backend does not use Http");
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
        if let Some(issue) = client.auth_issue() {
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

    // Route each note to the org that owns its repo. Notes whose repo isn't known
    // or isn't tracked by any org go to the home org (org = None → default token).
    type NoteBatchRow = (String, String, Option<String>);
    let mut groups: std::collections::HashMap<Option<String>, Vec<NoteBatchRow>> =
        std::collections::HashMap::new();
    for note in &pending {
        let org = note
            .repo_url
            .as_deref()
            .and_then(crate::api::client::resolve_org_for_repo_cached);
        groups.entry(org).or_default().push((
            note.commit_sha.clone(),
            note.content.clone(),
            note.repo_url.clone(),
        ));
    }

    for (org_opt, batch) in groups {
        let commit_shas: Vec<String> = batch.iter().map(|(sha, _, _)| sha.clone()).collect();
        let entries: Vec<NoteEntry> = batch
            .iter()
            .map(|(sha, content, repo_url)| NoteEntry {
                commit_sha: sha.clone(),
                content: content.clone(),
                repo_url: repo_url.clone(),
            })
            .collect();

        // Pick the client for this org. A resolved org mints an org-scoped token;
        // if minting fails we defer (mark failed → retry) rather than misroute.
        let group_client = match &org_opt {
            Some(org) => match crate::api::client::access_token_for_org(org) {
                Some(token) => {
                    ApiClient::new(ApiContext::with_auth(Some(backend_url.clone()), token))
                }
                None => {
                    if let Ok(db) = crate::notes::db::NotesDatabase::global()
                        && let Ok(mut lock) = db.lock()
                    {
                        let _ = lock.mark_failed(&commit_shas, "could not mint org-scoped token");
                    }
                    continue;
                }
            },
            None => ApiClient::new(ApiContext::new(Some(backend_url.clone()))),
        };

        let request = NotesUploadRequest { entries };
        match group_client.upload_notes(request) {
            Ok(resp) => {
                if resp.success_count > 0 {
                    note_durable_sync_authenticated();
                }
                tracing::debug!(
                    success = resp.success_count,
                    failure = resp.failure_count,
                    org = org_opt.as_deref().unwrap_or("home"),
                    "notes: uploaded batch"
                );
                if let Ok(db) = crate::notes::db::NotesDatabase::global()
                    && let Ok(mut lock) = db.lock()
                {
                    if resp.failure_count == 0 {
                        let _ = lock.mark_synced(&commit_shas);
                    } else {
                        // Server reported partial failures but doesn't identify which
                        // entries failed. Mark the whole group failed so all entries
                        // retry on the next flush cycle.
                        let _ = lock.mark_failed(
                            &commit_shas,
                            &format!(
                                "partial failure: {}/{} entries failed",
                                resp.failure_count,
                                commit_shas.len()
                            ),
                        );
                    }
                }
            }
            Err(e) => {
                note_durable_sync_upload_failed();
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
/// is active, send it to the same hosted data plane as notes (cli.autter.dev);
/// otherwise fall back to the API base URL (legacy behavior).
fn cas_client() -> (ApiClient, bool) {
    let cfg = Config::fresh();
    let dataplane_url = if cfg.notes_backend_kind().uses_http() {
        cfg.notes_backend_url().map(|s| s.to_string())
    } else {
        None
    };
    let context = ApiContext::new(dataplane_url);
    let target_url = context.base_url.clone();
    let client = ApiClient::new(context);

    let using_hosted = target_url == crate::config::DEFAULT_API_BASE_URL
        || target_url == crate::config::DEFAULT_NOTES_BACKEND_URL;
    let enabled = !using_hosted || client.is_logged_in() || client.has_api_key();
    (client, enabled)
}

fn flush_cas(records: Vec<CasSyncPayload>) {
    let (client, enabled) = cas_client();
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
                }
                let failed_hashes: Vec<String> = response
                    .results
                    .iter()
                    .filter(|result| result.status != "ok")
                    .map(|result| result.hash.clone())
                    .collect();
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
    note_durable_sync_upload_failed();
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
        if let Some(issue) = auth_client.auth_issue() {
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
    use super::panic_message;

    #[test]
    fn panic_message_reads_str_and_string_payloads() {
        let str_panic = std::panic::catch_unwind(|| panic!("boom")).unwrap_err();
        assert_eq!(panic_message(&str_panic), "boom");

        let string_panic = std::panic::catch_unwind(|| panic!("count is {}", 3)).unwrap_err();
        assert_eq!(panic_message(&string_panic), "count is 3");
    }
}
