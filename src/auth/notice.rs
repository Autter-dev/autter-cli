//! User-facing cloud-sync health reminders.
//!
//! Upload failures are otherwise invisible: the daemon logs warnings, but users
//! don't read daemon logs. This module surfaces stalled sync on:
//!
//! - every interactive `git commit` (post-commit footer — highest signal)
//! - other interactive git/autter commands (rate-limited to once per 24 hours)
//! - `autter whoami`, `autter doctor`, and `autter debug` (always)
//!
//! The daemon records auth-blocked and upload-failing stamps; interactive
//! commands read them back and show queue counts plus the fix.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::auth::CredentialStore;
use crate::auth::types::RefreshFailure;

/// Minimum seconds between notices across processes (24 hours).
const NOTICE_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// How long a daemon-reported auth failure stays "fresh" (48 hours). The
/// daemon re-records the stamp on every blocked flush attempt, so an active
/// problem keeps the stamp current; a stale stamp after a successful login
/// simply ages out even if the clear was missed.
const SYNC_BLOCKED_FRESH_SECS: i64 = 48 * 60 * 60;

static NOTICE_EMITTED: AtomicBool = AtomicBool::new(false);

#[derive(serde::Serialize, serde::Deserialize)]
struct MetricsReceipt {
    uploaded_at: i64,
    user_id: String,
    organization_slug: Option<String>,
}

fn metrics_receipt_path() -> PathBuf {
    crate::mdm::utils::home_dir()
        .join(".autter")
        .join("internal")
        .join("last-metrics-receipt.json")
}

pub fn record_metrics_upload(access_token: &str) {
    let identity = crate::auth::identity::extract_identity_from_access_token(access_token);
    let Some(user_id) = identity.user_id.as_ref() else {
        return;
    };
    let receipt = MetricsReceipt {
        uploaded_at: unix_now(),
        user_id: user_id.clone(),
        organization_slug: identity.active_org().and_then(|org| org.org_slug.clone()),
    };
    let receipt_path = metrics_receipt_path();
    if let Some(parent) = receipt_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_vec(&receipt) {
        let _ = std::fs::write(receipt_path, content);
    }
}

fn current_metrics_receipt() -> Option<MetricsReceipt> {
    let credentials = CredentialStore::new().load().ok().flatten()?;
    let identity =
        crate::auth::identity::extract_identity_from_access_token(&credentials.access_token);
    let content = std::fs::read(metrics_receipt_path()).ok()?;
    let receipt: MetricsReceipt = serde_json::from_slice(&content).ok()?;
    (identity.user_id.as_deref() == Some(receipt.user_id.as_str())).then_some(receipt)
}

fn notice_stamp_path() -> PathBuf {
    crate::mdm::utils::home_dir()
        .join(".autter")
        .join("internal")
        .join("logged-out-notice-at")
}

/// Holds the `at` of the last refused login the user was told about.
fn login_refused_notice_stamp_path() -> PathBuf {
    crate::mdm::utils::home_dir()
        .join(".autter")
        .join("internal")
        .join("login-refused-notice-at")
}

fn sync_blocked_stamp_path() -> PathBuf {
    crate::mdm::utils::home_dir()
        .join(".autter")
        .join("internal")
        .join("sync-auth-blocked-at")
}

fn sync_upload_stalled_stamp_path() -> PathBuf {
    crate::mdm::utils::home_dir()
        .join(".autter")
        .join("internal")
        .join("sync-upload-stalled-at")
}

/// Record that a sync attempt found pending work but no working auth.
/// Called by the daemon's flush loop; read back by [`maybe_warn_logged_out`].
/// This catches the case a pure token-expiry check misses: a refresh token
/// that is valid by timestamp but rejected by the server (revoked, rotated
/// signing keys, etc.).
pub fn record_sync_auth_blocked() {
    let path = sync_blocked_stamp_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, unix_now().to_string());
}

/// Clear the blocked stamp after a successful authenticated sync.
/// Does not clear an upload failure: a reachable login is not a successful
/// drain of the queue that just failed.
pub fn clear_sync_auth_blocked() {
    let _ = std::fs::remove_file(sync_blocked_stamp_path());
}

/// One durable queue's most recent non-auth upload failure.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UploadFailure {
    pub at: i64,
    pub queue: String,
    pub detail: String,
}

fn upload_failure_path() -> PathBuf {
    sync_upload_stalled_stamp_path().with_file_name("sync-upload-failure.json")
}

/// Record that a durable-queue upload failed for a reason other than missing
/// auth (network error, rejected batch, org database unreachable, etc.).
///
/// Usage metrics do not go through here: their failures live in
/// [`crate::metrics::sync_state`] and are reported as "usage metrics", since a
/// metrics problem says nothing about whether attribution reaches autter.
pub fn record_upload_failure(queue: &str, detail: &str) {
    if queue == "metrics" {
        return;
    }
    let detail: String = detail.trim().chars().take(300).collect();
    let failure = UploadFailure {
        at: unix_now(),
        queue: queue.to_string(),
        detail: if detail.is_empty() {
            "upload failed".to_string()
        } else {
            detail
        },
    };
    let path = upload_failure_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_vec(&failure) {
        let _ = std::fs::write(&path, content);
    }
    record_sync_upload_stalled();
}

/// Drop the recorded failure when that same queue uploads successfully.
pub fn clear_upload_failure_for(queue: &str) {
    let Some(current) = read_upload_failure() else {
        return;
    };
    if current.queue != queue {
        return;
    }
    clear_upload_failures();
}

/// Drop every upload-failure stamp, including the legacy timestamp files and
/// the usage-metrics failure state.
pub fn clear_upload_failures() {
    let _ = std::fs::remove_file(upload_failure_path());
    clear_sync_upload_stalled();
    clear_metrics_upload_stalled();
    crate::metrics::sync_state::MetricsSyncState::reset();
}

fn read_upload_failure() -> Option<UploadFailure> {
    let content = std::fs::read(upload_failure_path()).ok()?;
    serde_json::from_slice(&content).ok()
}

/// The fresh attribution-sync upload failure (notes, commit summaries,
/// transcripts, file changes), if the background service recorded one.
/// Usage-metrics failures are reported separately
/// ([`crate::metrics::sync_state::UsageMetricsStatus`]); a metrics record
/// left by an older version is ignored here.
pub fn current_upload_failure() -> Option<UploadFailure> {
    match read_upload_failure() {
        Some(failure) if failure.queue == "metrics" => return None,
        Some(failure) if unix_now() - failure.at < SYNC_BLOCKED_FRESH_SECS => {
            return Some(failure);
        }
        _ => {}
    }
    // Legacy stamp: older versions wrote a bare timestamp for every queue,
    // and a separate one for metrics. When the newest is the metrics one, it
    // was a metrics failure.
    let metrics_at = stamp_time(&metrics_upload_stalled_stamp_path());
    let at = stamp_time(&sync_upload_stalled_stamp_path())?;
    if metrics_at.is_some_and(|metrics| metrics >= at) {
        return None;
    }
    (unix_now() - at < SYNC_BLOCKED_FRESH_SECS).then(|| UploadFailure {
        at,
        queue: "upload".to_string(),
        detail: "the background upload failed; no error was recorded".to_string(),
    })
}

fn stamp_time(path: &std::path::Path) -> Option<i64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
}

/// Record that a durable-queue upload failed for a reason other than missing
/// auth (network error, org database unreachable, etc.).
fn record_sync_upload_stalled() {
    let path = sync_upload_stalled_stamp_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, unix_now().to_string());
}

/// Clear the upload-stalled stamp after a successful cloud upload.
pub fn clear_sync_upload_stalled() {
    let _ = std::fs::remove_file(sync_upload_stalled_stamp_path());
}

fn metrics_upload_stalled_stamp_path() -> PathBuf {
    sync_upload_stalled_stamp_path().with_file_name("metrics-upload-stalled-at")
}

fn clear_metrics_upload_stalled() {
    let _ = std::fs::remove_file(metrics_upload_stalled_stamp_path());
}

/// True when the daemon recently reported auth-blocked sync attempts.
pub fn sync_auth_blocked_recently() -> bool {
    let Ok(raw) = std::fs::read_to_string(sync_blocked_stamp_path()) else {
        return false;
    };
    let Ok(at) = raw.trim().parse::<i64>() else {
        return false;
    };
    unix_now() - at < SYNC_BLOCKED_FRESH_SECS
}

/// True when the daemon recently reported non-auth upload failures.
pub fn sync_upload_stalled_recently() -> bool {
    current_upload_failure().is_some()
}

/// Why cloud sync needs user attention, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudSyncAttention {
    /// Session expired or the server rejected stored credentials.
    AuthBlocked,
    /// Logged in, but uploads are failing (network / org database / etc.).
    UploadFailing,
    /// Background service is not running, so nothing can drain the queues.
    DaemonNotRunning,
}

/// Whether cloud upload is on. Same predicate the uploaders check, so the
/// status line can never say "off" while data is being sent.
fn cloud_sync_enabled() -> bool {
    crate::config::platform_sync_enabled_now()
}

fn background_service_running() -> bool {
    if std::env::var_os("AUTTER_TEST_DB_PATH").is_some() {
        return true;
    }
    let Ok(config) = crate::daemon::DaemonConfig::from_env_or_default_paths() else {
        return false;
    };
    // The same probe `autter doctor` and `autter bg status` use, so whoami and
    // status can't call a busy service "stopped" while doctor calls it
    // running (the old 100ms readiness check did exactly that).
    use crate::commands::daemon::{DaemonHealthState, probe_daemon_health};
    matches!(
        probe_daemon_health(&config).state,
        DaemonHealthState::Healthy | DaemonHealthState::Outdated
    )
}

/// Detect whether the user should be told that cloud upload is not working.
pub fn cloud_sync_attention() -> Option<CloudSyncAttention> {
    if !cloud_sync_enabled() {
        return None;
    }

    let creds = CredentialStore::new().load().ok().flatten();
    let auth_blocked = creds.as_ref().is_none_or(|credentials| {
        credentials.is_refresh_token_expired() || credentials.is_login_required()
    }) || sync_auth_blocked_recently();

    if auth_blocked {
        return Some(CloudSyncAttention::AuthBlocked);
    }

    if !background_service_running() {
        return Some(CloudSyncAttention::DaemonNotRunning);
    }

    if sync_upload_stalled_recently() {
        return Some(CloudSyncAttention::UploadFailing);
    }

    None
}

/// Serializable pending-queue counts for `autter sync status` and `autter bg status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct PendingSyncCountsJson {
    pub metrics: i64,
    pub notes: i64,
    pub commit_summaries: i64,
    pub transcripts: i64,
    pub file_changes: i64,
    pub total: i64,
}

/// Machine-readable cloud-sync health for status commands.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CloudSyncStatusReport {
    /// Whether this machine uploads to autter cloud (connected mode). Matches
    /// what every uploader checks: when false, nothing is sent.
    pub enabled: bool,
    pub daemon_running: bool,
    /// Overall state: healthy, draining a backlog, or blocked.
    pub state: CloudSyncState,
    pub pending: PendingSyncCountsJson,
    /// Items held until the user consents to upload them (backlog from an
    /// earlier autter version, or from before connecting). Never drained
    /// automatically; see `crate::upload_hold`.
    pub held: crate::upload_hold::QueueCounts,
    pub queue_status_available: bool,
    pub auth_blocked_recently: bool,
    pub upload_stalled_recently: bool,
    /// Which queue failed, and the error the worker recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_failure: Option<UploadFailure>,
    /// Usage metrics (telemetry events), reported apart from attribution: a
    /// failing metrics upload does not make cloud sync "failing".
    pub usage_metrics: crate::metrics::sync_state::UsageMetricsStatus,
    pub last_metrics_upload_at: Option<i64>,
    pub organization_slug: Option<String>,
    pub dashboard_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudSyncState {
    Disabled,
    Healthy,
    Draining,
    AuthBlocked,
    UploadFailing,
    DaemonNotRunning,
    StatusUnavailable,
}

/// Collect the current cloud-sync picture for status commands.
pub fn collect_cloud_sync_status() -> CloudSyncStatusReport {
    let enabled = cloud_sync_enabled();
    let daemon_running = background_service_running();
    let queue_counts = read_pending_sync_counts();
    let queue_status_available = queue_counts.is_some();
    let pending = queue_counts.unwrap_or_default();
    let pending_json = PendingSyncCountsJson::from(pending);
    let auth_blocked_recently = sync_auth_blocked_recently();
    let upload_failure = current_upload_failure();
    let upload_stalled_recently = upload_failure.is_some();
    let attention = cloud_sync_attention();

    let state = if !enabled {
        CloudSyncState::Disabled
    } else if let Some(attention) = attention {
        match attention {
            CloudSyncAttention::AuthBlocked => CloudSyncState::AuthBlocked,
            CloudSyncAttention::UploadFailing => CloudSyncState::UploadFailing,
            CloudSyncAttention::DaemonNotRunning => CloudSyncState::DaemonNotRunning,
        }
    } else if !queue_status_available {
        CloudSyncState::StatusUnavailable
    } else if pending.total() > 0 {
        CloudSyncState::Draining
    } else {
        CloudSyncState::Healthy
    };

    let remediation = upload_failure
        .as_ref()
        .map(|failure| {
            format!(
                "{} upload failed: {}. Run `autter doctor`, then `autter bg restart`",
                failure.queue, failure.detail
            )
        })
        .or_else(|| attention.map(remediation_for))
        .or_else(|| {
            (state == CloudSyncState::StatusUnavailable)
                .then(|| "run `autter doctor` to check the local upload queue".to_string())
        });
    let receipt = current_metrics_receipt();
    let last_metrics_upload_at = receipt.as_ref().map(|receipt| receipt.uploaded_at);
    let organization_slug = receipt
        .and_then(|receipt| receipt.organization_slug)
        .or_else(|| {
            let credentials = CredentialStore::new().load().ok().flatten()?;
            let identity = crate::auth::identity::extract_identity_from_access_token(
                &credentials.access_token,
            );
            identity.active_org().and_then(|org| org.org_slug.clone())
        });
    let dashboard_url = organization_slug.as_deref().and_then(|slug| {
        let mut url = url::Url::parse(&crate::commands::login::web_app_url()).ok()?;
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .push(slug)
            .push("provenance");
        Some(url.to_string())
    });

    CloudSyncStatusReport {
        enabled,
        daemon_running,
        state,
        pending: pending_json,
        held: crate::upload_hold::held_counts(),
        queue_status_available,
        auth_blocked_recently,
        upload_stalled_recently,
        upload_failure,
        usage_metrics: crate::metrics::sync_state::UsageMetricsStatus::current(pending.metrics),
        last_metrics_upload_at,
        organization_slug,
        dashboard_url,
        remediation,
    }
}

fn remediation_for(attention: CloudSyncAttention) -> String {
    match attention {
        CloudSyncAttention::AuthBlocked => {
            "run `autter login`, then `autter doctor` to verify".to_string()
        }
        CloudSyncAttention::UploadFailing => {
            "run `autter doctor` (checks network + org database), then `autter bg restart`"
                .to_string()
        }
        CloudSyncAttention::DaemonNotRunning => {
            "run `autter bg restart`, then `autter doctor` to verify".to_string()
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// True when the notice was shown within the last [`NOTICE_INTERVAL_SECS`].
fn recently_notified() -> bool {
    let Ok(raw) = std::fs::read_to_string(notice_stamp_path()) else {
        return false;
    };
    let Ok(last) = raw.trim().parse::<i64>() else {
        return false;
    };
    unix_now() - last < NOTICE_INTERVAL_SECS
}

fn record_notified() {
    let path = notice_stamp_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, unix_now().to_string());
}

/// Counts of locally queued items waiting for cloud upload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingSyncCounts {
    pub metrics: i64,
    pub notes: i64,
    pub commit_summaries: i64,
    pub transcripts: i64,
    pub file_changes: i64,
}

impl From<PendingSyncCounts> for PendingSyncCountsJson {
    fn from(counts: PendingSyncCounts) -> Self {
        Self {
            metrics: counts.metrics,
            notes: counts.notes,
            commit_summaries: counts.commit_summaries,
            transcripts: counts.transcripts,
            file_changes: counts.file_changes,
            total: counts.total(),
        }
    }
}

impl PendingSyncCounts {
    pub fn total(self) -> i64 {
        self.metrics + self.notes + self.commit_summaries + self.transcripts + self.file_changes
    }

    pub fn summary(self) -> String {
        format!(
            "{} telemetry events, {} authorship notes, {} commit summaries, {} transcripts, {} file-change records",
            self.metrics, self.notes, self.commit_summaries, self.transcripts, self.file_changes
        )
    }

    /// The same counts without usage metrics, which are reported on their own.
    pub fn attribution_only(self) -> Self {
        Self { metrics: 0, ..self }
    }

    /// [`Self::summary`] without the usage-metrics count.
    pub fn attribution_summary(self) -> String {
        format!(
            "{} authorship notes, {} commit summaries, {} transcripts, {} file-change records",
            self.notes, self.commit_summaries, self.transcripts, self.file_changes
        )
    }
}

/// Count every durable local queue used by cloud sync. Best-effort: an
/// unavailable database contributes zero instead of breaking diagnostics.
pub fn pending_sync_counts() -> PendingSyncCounts {
    read_pending_sync_counts().unwrap_or_default()
}

fn read_pending_sync_counts() -> Option<PendingSyncCounts> {
    let metrics = crate::metrics::db::MetricsDatabase::global()
        .ok()
        .and_then(|db| db.lock().ok()?.count().ok())? as i64;
    let notes = crate::notes::db::NotesDatabase::global()
        .ok()
        .and_then(|db| db.lock().ok()?.count_pending().ok())?;
    let commit_summaries = crate::notes::db::NotesDatabase::global()
        .ok()
        .and_then(|db| db.lock().ok()?.count_pending_commit_summaries().ok())?;
    let transcripts = crate::authorship::internal_db::InternalDatabase::global()
        .ok()
        .and_then(|db| db.lock().ok()?.count_pending_cas().ok())?;
    let file_changes = crate::file_changes::FileChangesDatabase::global()
        .ok()
        .and_then(|db| db.lock().ok()?.count_pending().ok())?;
    // Items parked because their repo isn't connected aren't waiting on the
    // upload pipeline; counting them would make a healthy sync look stuck.
    let kept = crate::upload_hold::unconnected_counts();
    Some(PendingSyncCounts {
        metrics,
        notes: (notes - kept.notes).max(0),
        commit_summaries: (commit_summaries - kept.commit_summaries).max(0),
        transcripts: (transcripts - kept.transcripts).max(0),
        file_changes: (file_changes - kept.file_changes).max(0),
    })
}

/// The server's refusal of the stored login, if it refused it.
fn stored_login_refusal() -> Option<RefreshFailure> {
    CredentialStore::new().load().ok().flatten()?.refresh_failed
}

fn print_sync_attention_message(
    attention: CloudSyncAttention,
    pending: PendingSyncCounts,
    refusal: Option<&RefreshFailure>,
) {
    eprintln!();
    match attention {
        CloudSyncAttention::AuthBlocked => {
            if let Some(refusal) = refusal {
                eprintln!(
                    "\x1b[1;33m⚠ Cloud sync is paused — the autter server refused your saved login.\x1b[0m"
                );
                eprintln!("\x1b[1;33m  {}\x1b[0m", refusal.message());
            } else {
                eprintln!(
                    "\x1b[1;33m⚠ Cloud sync is paused — your autter login has expired.\x1b[0m"
                );
            }
            if pending.total() > 0 {
                eprintln!(
                    "\x1b[1;33m  {} are stored locally and will upload once you're back in.\x1b[0m",
                    pending.summary(),
                );
            }
            eprintln!(
                "\x1b[1;33m  Fix: run \x1b[1;36mautter login\x1b[0m\x1b[1;33m, then \x1b[1;36mautter doctor\x1b[0m\x1b[1;33m to verify.\x1b[0m"
            );
        }
        CloudSyncAttention::UploadFailing => {
            eprintln!(
                "\x1b[1;33m⚠ Cloud sync is failing — your data is not reaching autter.\x1b[0m"
            );
            if let Some(failure) = current_upload_failure() {
                eprintln!(
                    "\x1b[1;33m  Failing queue: {} — {}\x1b[0m",
                    failure.queue, failure.detail
                );
            }
            if pending.total() > 0 {
                eprintln!(
                    "\x1b[1;33m  {} are queued locally.\x1b[0m",
                    pending.summary(),
                );
            } else {
                eprintln!(
                    "\x1b[1;33m  Recent uploads failed; new commits may not appear in the dashboard.\x1b[0m"
                );
            }
            eprintln!(
                "\x1b[1;33m  Fix: run \x1b[1;36mautter doctor\x1b[0m\x1b[1;33m, then \x1b[1;36mautter bg restart\x1b[0m\x1b[1;33m.\x1b[0m"
            );
        }
        CloudSyncAttention::DaemonNotRunning => {
            eprintln!(
                "\x1b[1;33m⚠ Cloud sync is paused — the autter background service is not running.\x1b[0m"
            );
            eprintln!(
                "\x1b[1;33m  {} are queued locally.\x1b[0m",
                pending.summary(),
            );
            eprintln!(
                "\x1b[1;33m  Fix: run \x1b[1;36mautter bg restart\x1b[0m\x1b[1;33m, then \x1b[1;36mautter doctor\x1b[0m\x1b[1;33m to verify.\x1b[0m"
            );
        }
    }
    eprintln!();
}

fn eprint_sync_attention(rate_limited: bool) {
    if !std::io::stdout().is_terminal() {
        return;
    }
    let Some(attention) = cloud_sync_attention() else {
        return;
    };
    let refusal = stored_login_refusal();
    // A login the server just refused is news: say so on the next command even
    // inside the 24-hour reminder window, but only once per refusal.
    if attention == CloudSyncAttention::AuthBlocked
        && let Some(refusal) = &refusal
        && !refusal_already_announced(&login_refused_notice_stamp_path(), refusal)
    {
        announce_login_refused(refusal);
        return;
    }
    if rate_limited {
        if NOTICE_EMITTED.load(Ordering::Relaxed) || recently_notified() {
            return;
        }
        if NOTICE_EMITTED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        record_notified();
    }

    let pending = pending_sync_counts();
    print_sync_attention_message(attention, pending, refusal.as_ref());
}

/// Tell an interactive user, once per refusal, that the server refused their
/// saved login, with the server's own reason. The daemon has no terminal, so
/// its refusals reach the user through the next interactive command instead
/// (see [`maybe_warn_logged_out`]).
pub fn announce_login_refused(refusal: &RefreshFailure) {
    if cfg!(test) || !std::io::stdout().is_terminal() {
        return;
    }
    let stamp = login_refused_notice_stamp_path();
    if refusal_already_announced(&stamp, refusal) {
        return;
    }
    if NOTICE_EMITTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    record_refusal_announced(&stamp, refusal);
    record_notified();
    print_sync_attention_message(
        CloudSyncAttention::AuthBlocked,
        pending_sync_counts(),
        Some(refusal),
    );
}

fn refusal_already_announced(stamp: &std::path::Path, refusal: &RefreshFailure) -> bool {
    stamp_time(stamp) == Some(refusal.at)
}

fn record_refusal_announced(stamp: &std::path::Path, refusal: &RefreshFailure) {
    if let Some(parent) = stamp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(stamp, refusal.at.to_string());
}

/// Rate-limited reminder on interactive git/autter commands (once per 24 hours).
pub fn maybe_warn_logged_out() {
    eprint_sync_attention(true);
}

/// Unconditional reminder right after `git commit` — the moment users care most.
pub fn eprint_post_commit_sync_reminder() {
    if std::io::stdout().is_terminal()
        && !crate::commands::arg_parser::quiet()
        && !crate::commands::arg_parser::json()
    {
        eprintln!("Local commit recorded.");
        if let Some(line) = format_cloud_sync_status_line() {
            eprintln!("{line}");
        }
    }
}

/// Human-readable cloud-sync summary for `whoami` / `doctor` / `debug`.
pub fn format_cloud_sync_status_line() -> Option<String> {
    let report = collect_cloud_sync_status();
    Some(format_sync_report(&report))
}

pub fn format_sync_report(report: &CloudSyncStatusReport) -> String {
    let detail = match report.state {
        CloudSyncState::Disabled => "off; records stay on this computer. Connect: `autter onboard`",
        CloudSyncState::AuthBlocked => "blocked; sign in with `autter login`",
        CloudSyncState::UploadFailing => "upload failed; run `autter doctor`",
        CloudSyncState::DaemonNotRunning => "background service stopped; run `autter bg restart`",
        CloudSyncState::Draining => "upload pending; check `autter sync status`",
        CloudSyncState::Healthy => "no queued uploads; open the dashboard with `autter sync open`",
        CloudSyncState::StatusUnavailable => "queue status unavailable; run `autter doctor`",
    };
    let mut summary = format!("Cloud upload: {detail}");
    if report.enabled && report.pending.total > 0 {
        summary.push_str(&format!(" ({} records waiting)", report.pending.total));
    }
    if report.held.total() > 0 {
        summary.push_str(&format!(
            "\n{} records queued before you connected (or by an older autter) are held and will not upload. Upload: `{}`. Delete: `{}`.",
            report.held.total(),
            crate::upload_hold::IMPORT_COMMAND,
            crate::upload_hold::PURGE_COMMAND
        ));
    }
    if report.state == CloudSyncState::UploadFailing
        && let Some(failure) = &report.upload_failure
    {
        summary.push_str(&format!(
            "\nFailing queue: {} — {}",
            failure.queue, failure.detail
        ));
    }
    if report.enabled {
        if report.usage_metrics.queued > 0 || report.usage_metrics.failing {
            summary.push('\n');
            summary.push_str(&report.usage_metrics.summary_line());
            if report.usage_metrics.failing {
                summary.push_str(
                    "\nUsage metrics are separate from attribution; this does not stop authorship notes from uploading.",
                );
            }
        }
        if let Some(uploaded_at) = report.last_metrics_upload_at {
            summary.push_str(&format!(
                "\nLast metrics batch received: {}",
                crate::auth::format_unix_timestamp(uploaded_at),
            ));
        } else {
            summary.push_str("\nNo upload receipt recorded on this computer.");
        }
        if let Some(organization_slug) = report.organization_slug.as_deref() {
            summary.push_str(&format!("\nDashboard organization: {organization_slug}"));
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_report(state: CloudSyncState) -> CloudSyncStatusReport {
        CloudSyncStatusReport {
            enabled: state != CloudSyncState::Disabled,
            daemon_running: true,
            state,
            pending: PendingSyncCountsJson::from(PendingSyncCounts::default()),
            held: crate::upload_hold::QueueCounts::default(),
            queue_status_available: state != CloudSyncState::StatusUnavailable,
            auth_blocked_recently: false,
            upload_stalled_recently: false,
            upload_failure: None,
            usage_metrics: crate::metrics::sync_state::UsageMetricsStatus::from_state(
                &crate::metrics::sync_state::MetricsSyncState::default(),
                0,
                0,
            ),
            last_metrics_upload_at: None,
            organization_slug: None,
            dashboard_url: None,
            remediation: None,
        }
    }

    #[test]
    fn empty_queue_does_not_claim_confirmed_upload() {
        let summary = format_sync_report(&status_report(CloudSyncState::Healthy));
        assert!(summary.contains("no queued uploads"));
        assert!(summary.contains("No upload receipt recorded"));
        assert!(!summary.contains("Last metrics batch received"));
    }

    #[test]
    fn blocked_states_show_the_next_action() {
        for (state, command) in [
            (CloudSyncState::Disabled, "autter onboard"),
            (CloudSyncState::AuthBlocked, "autter login"),
            (CloudSyncState::UploadFailing, "autter doctor"),
            (CloudSyncState::DaemonNotRunning, "autter bg restart"),
            (CloudSyncState::StatusUnavailable, "autter doctor"),
        ] {
            assert!(format_sync_report(&status_report(state)).contains(command));
        }
    }

    #[test]
    fn upload_failure_names_the_queue_and_error() {
        let mut report = status_report(CloudSyncState::UploadFailing);
        report.upload_failure = Some(UploadFailure {
            at: 1,
            queue: "notes".to_string(),
            detail: "batch not fully accepted (2 rejected)".to_string(),
        });
        let summary = format_sync_report(&report);
        assert!(summary.contains("autter doctor"));
        assert!(summary.contains("Failing queue: notes"));
        assert!(summary.contains("batch not fully accepted (2 rejected)"));
    }

    #[test]
    fn failing_usage_metrics_show_counts_and_the_last_error_without_failing_sync() {
        use crate::metrics::sync_state::{MetricsSyncState, UsageMetricsStatus};
        let mut state = MetricsSyncState {
            uploaded_total: 1_200,
            ..MetricsSyncState::default()
        };
        state.record_failure(
            1_762_588_800,
            &crate::api::metrics::MetricsUploadFailure::Timeout {
                after_secs: Some(30),
            },
            643,
        );
        let mut report = status_report(CloudSyncState::Draining);
        report.pending.metrics = 643;
        report.pending.total = 643;
        report.usage_metrics = UsageMetricsStatus::from_state(&state, 643, 1_762_588_800);
        let summary = format_sync_report(&report);
        assert!(summary.contains("upload pending"), "{summary}");
        assert!(!summary.contains("upload failed"), "{summary}");
        assert!(
            summary.contains("Usage metrics: 643 queued, 1200 uploaded, 1 failed attempt"),
            "{summary}"
        );
        assert!(
            summary.contains(
                "Last metrics error: POST /worker/metrics/upload: timeout after 30s (no response)"
            ),
            "{summary}"
        );
        assert!(summary.contains("next: 321 events"), "{summary}");
        assert!(!summary.contains("bg restart"), "{summary}");
        assert!(summary.contains("separate from attribution"), "{summary}");
    }

    #[test]
    fn pending_upload_remains_distinct_from_an_earlier_receipt() {
        let mut report = status_report(CloudSyncState::Draining);
        report.pending.metrics = 2;
        report.pending.total = 2;
        report.last_metrics_upload_at = Some(1_735_689_600);
        let summary = format_sync_report(&report);
        assert!(summary.contains("upload pending"));
        assert!(summary.contains("2 records waiting"));
        assert!(summary.contains("Last metrics batch received"));
    }

    #[test]
    fn login_refusal_is_announced_once_per_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join("login-refused-notice-at");
        let refusal = RefreshFailure {
            at: 1_760_000_000,
            error: "invalid_grant".to_string(),
            error_code: Some("refresh_token_invalid".to_string()),
            description: Some("Refresh token is not valid.".to_string()),
        };

        assert!(!refusal_already_announced(&stamp, &refusal));
        record_refusal_announced(&stamp, &refusal);
        assert!(refusal_already_announced(&stamp, &refusal));
        assert!(refusal_already_announced(&stamp, &refusal.clone()));

        // A new login that is refused later is news again.
        let later = RefreshFailure {
            at: refusal.at + 3600,
            ..refusal
        };
        assert!(!refusal_already_announced(&stamp, &later));
    }

    #[test]
    fn notice_interval_parse_roundtrip() {
        // The stamp file format is a bare unix timestamp; make sure the
        // parse used by recently_notified accepts what record_notified writes.
        let now = unix_now();
        let parsed = now.to_string().trim().parse::<i64>().unwrap();
        assert_eq!(parsed, now);
        assert!(now > 0);
    }
}
