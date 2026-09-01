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
    let Ok(raw) = std::fs::read_to_string(sync_upload_stalled_stamp_path()) else {
        return false;
    };
    let Ok(at) = raw.trim().parse::<i64>() else {
        return false;
    };
    unix_now() - at < SYNC_BLOCKED_FRESH_SECS
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
    let auth_blocked = creds.as_ref().is_some_and(|c| c.is_refresh_token_expired())
        || sync_auth_blocked_recently();

    if auth_blocked && (creds.is_some() || sync_auth_blocked_recently()) {
        return Some(CloudSyncAttention::AuthBlocked);
    }

    if !background_service_running() {
        let pending = pending_sync_counts();
        if pending.total() > 0 {
            return Some(CloudSyncAttention::DaemonNotRunning);
        }
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
    pub auth_blocked_recently: bool,
    pub upload_stalled_recently: bool,
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
}

/// Collect the current cloud-sync picture for status commands.
pub fn collect_cloud_sync_status() -> CloudSyncStatusReport {
    let enabled = cloud_sync_enabled();
    let daemon_running = background_service_running();
    let pending = pending_sync_counts();
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
    } else if pending.total() > 0 {
        CloudSyncState::Draining
    } else {
        CloudSyncState::Healthy
    };

    let remediation = attention.map(remediation_for);

    CloudSyncStatusReport {
        enabled,
        daemon_running,
        state,
        pending: pending_json,
        auth_blocked_recently,
        upload_stalled_recently,
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
    let metrics = crate::metrics::db::MetricsDatabase::global()
        .ok()
        .and_then(|db| db.lock().ok().map(|lock| lock.count().unwrap_or(0) as i64))
        .unwrap_or(0);
    let notes = crate::notes::db::NotesDatabase::global()
        .ok()
        .and_then(|db| db.lock().ok().map(|lock| lock.count_pending().unwrap_or(0)))
        .unwrap_or(0);
    let commit_summaries = crate::notes::db::NotesDatabase::global()
        .ok()
        .and_then(|db| {
            db.lock()
                .ok()
                .map(|lock| lock.count_pending_commit_summaries().unwrap_or(0))
        })
        .unwrap_or(0);
    let transcripts = crate::authorship::internal_db::InternalDatabase::global()
        .ok()
        .and_then(|db| {
            db.lock()
                .ok()
                .map(|lock| lock.count_pending_cas().unwrap_or(0))
        })
        .unwrap_or(0);
    let file_changes = crate::file_changes::FileChangesDatabase::global()
        .ok()
        .and_then(|db| db.lock().ok().map(|lock| lock.count_pending().unwrap_or(0)))
        .unwrap_or(0);
    PendingSyncCounts {
        metrics,
        notes,
        commit_summaries,
        transcripts,
        file_changes,
    }
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
    eprint_sync_attention(false);
}

/// Human-readable cloud-sync summary for `whoami` / `doctor` / `debug`.
pub fn format_cloud_sync_status_line() -> Option<String> {
    let report = collect_cloud_sync_status();
    if !report.enabled {
        return None;
    }
    let detail = match report.state {
        CloudSyncState::AuthBlocked => "login expired — run `autter login`",
        CloudSyncState::UploadFailing => "uploads failing — run `autter doctor`",
        CloudSyncState::DaemonNotRunning => {
            "background service not running — run `autter bg start`"
        }
        CloudSyncState::Draining => "upload in progress",
        CloudSyncState::Healthy | CloudSyncState::Disabled => return None,
    };
    if report.pending.total > 0 {
        Some(format!(
            "Cloud sync: {detail} ({})",
            PendingSyncCounts {
                metrics: report.pending.metrics,
                notes: report.pending.notes,
                commit_summaries: report.pending.commit_summaries,
                transcripts: report.pending.transcripts,
                file_changes: report.pending.file_changes,
            }
            .summary()
        ))
    } else {
        Some(format!("Cloud sync: {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
