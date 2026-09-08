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
    let identity = crate::auth::identity::extract_identity_from_access_token(&credentials.access_token);
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
pub fn clear_sync_auth_blocked() {
    let _ = std::fs::remove_file(sync_blocked_stamp_path());
    clear_sync_upload_stalled();
}

/// Record that a durable-queue upload failed for a reason other than missing
/// auth (network error, org database unreachable, etc.).
pub fn record_sync_upload_stalled() {
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

pub fn record_metrics_upload_stalled() {
    let stamp = sync_upload_stalled_stamp_path().with_file_name("metrics-upload-stalled-at");
    if let Some(parent) = stamp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(stamp, unix_now().to_string());
}

pub fn clear_metrics_upload_stalled() {
    let stamp = sync_upload_stalled_stamp_path().with_file_name("metrics-upload-stalled-at");
    let _ = std::fs::remove_file(stamp);
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
    [
        sync_upload_stalled_stamp_path(),
        sync_upload_stalled_stamp_path().with_file_name("metrics-upload-stalled-at"),
    ]
    .iter()
    .any(|stamp| {
        std::fs::read_to_string(stamp)
            .ok()
            .and_then(|raw| raw.trim().parse::<i64>().ok())
            .is_some_and(|at| unix_now() - at < SYNC_BLOCKED_FRESH_SECS)
    })
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

fn cloud_sync_enabled() -> bool {
    crate::config::Config::fresh()
        .notes_backend_kind()
        .uses_http()
}

fn background_service_running() -> bool {
    if std::env::var_os("AUTTER_TEST_DB_PATH").is_some() {
        return true;
    }
    let Ok(config) = crate::daemon::DaemonConfig::from_env_or_default_paths() else {
        return false;
    };
    crate::commands::daemon::daemon_is_up(&config)
}

/// Detect whether the user should be told that cloud upload is not working.
pub fn cloud_sync_attention() -> Option<CloudSyncAttention> {
    if !cloud_sync_enabled() {
        return None;
    }

    let creds = CredentialStore::new().load().ok().flatten();
    let auth_blocked = creds.as_ref().is_none_or(|credentials| credentials.is_refresh_token_expired())
        || sync_auth_blocked_recently();

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
    /// Whether the configured notes backend uploads to autter cloud.
    pub enabled: bool,
    pub daemon_running: bool,
    /// Overall state: healthy, draining a backlog, or blocked.
    pub state: CloudSyncState,
    pub pending: PendingSyncCountsJson,
    pub queue_status_available: bool,
    pub auth_blocked_recently: bool,
    pub upload_stalled_recently: bool,
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
    let upload_stalled_recently = sync_upload_stalled_recently();
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

    let remediation = attention.map(remediation_for).or_else(|| {
        (state == CloudSyncState::StatusUnavailable)
            .then(|| "run `autter doctor` to check the local upload queue".to_string())
    });
    let receipt = current_metrics_receipt();
    let last_metrics_upload_at = receipt.as_ref().map(|receipt| receipt.uploaded_at);
    let organization_slug = receipt.and_then(|receipt| receipt.organization_slug).or_else(|| {
        let credentials = CredentialStore::new().load().ok().flatten()?;
        let identity = crate::auth::identity::extract_identity_from_access_token(&credentials.access_token);
        identity.active_org().and_then(|org| org.org_slug.clone())
    });
    let dashboard_url = organization_slug.as_deref().and_then(|slug| {
        let mut url = url::Url::parse(&crate::commands::login::web_app_url()).ok()?;
        url.path_segments_mut().ok()?.pop_if_empty().push(slug).push("provenance");
        Some(url.to_string())
    });

    CloudSyncStatusReport {
        enabled,
        daemon_running,
        state,
        pending: pending_json,
        queue_status_available,
        auth_blocked_recently,
        upload_stalled_recently,
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
            "run `autter bg start`, then `autter doctor` to verify".to_string()
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
    Some(PendingSyncCounts {
        metrics,
        notes,
        commit_summaries,
        transcripts,
        file_changes,
    })
}

fn print_sync_attention_message(attention: CloudSyncAttention, pending: PendingSyncCounts) {
    eprintln!();
    match attention {
        CloudSyncAttention::AuthBlocked => {
            eprintln!("\x1b[1;33m⚠ Cloud sync is paused — your autter login has expired.\x1b[0m");
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
                "\x1b[1;33m  Fix: run \x1b[1;36mautter doctor\x1b[0m\x1b[1;33m (checks network + org database), then \x1b[1;36mautter bg restart\x1b[0m\x1b[1;33m.\x1b[0m"
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
                "\x1b[1;33m  Fix: run \x1b[1;36mautter bg start\x1b[0m\x1b[1;33m, then \x1b[1;36mautter doctor\x1b[0m\x1b[1;33m to verify.\x1b[0m"
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
    print_sync_attention_message(attention, pending);
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
        CloudSyncState::DaemonNotRunning => {
            "background service stopped; run `autter bg start`"
        }
        CloudSyncState::Draining => "upload pending; check `autter sync status`",
        CloudSyncState::Healthy => "no queued uploads; open the dashboard with `autter sync open`",
        CloudSyncState::StatusUnavailable => "queue status unavailable; run `autter doctor`",
    };
    let mut summary = format!("Cloud upload: {detail}");
    if report.enabled && report.pending.total > 0 {
        summary.push_str(&format!(" ({} records waiting)", report.pending.total));
    }
    if report.enabled {
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
            queue_status_available: state != CloudSyncState::StatusUnavailable,
            auth_blocked_recently: false,
            upload_stalled_recently: false,
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
            (CloudSyncState::DaemonNotRunning, "autter bg start"),
            (CloudSyncState::StatusUnavailable, "autter doctor"),
        ] {
            assert!(format_sync_report(&status_report(state)).contains(command));
        }
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
    fn notice_interval_parse_roundtrip() {
        // The stamp file format is a bare unix timestamp; make sure the
        // parse used by recently_notified accepts what record_notified writes.
        let now = unix_now();
        let parsed = now.to_string().trim().parse::<i64>().unwrap();
        assert_eq!(parsed, now);
        assert!(now > 0);
    }
}
