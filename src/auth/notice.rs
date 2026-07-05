//! User-facing "you've been logged out" reminder.
//!
//! When a login expires (refresh token past its lifetime), cloud sync of
//! authorship notes and prompt transcripts silently pauses — queued data stays
//! local until the user logs in again. The daemon logs a warning, but users
//! don't read daemon logs, so this surfaces the state on interactive commands:
//! a short stderr notice with the pending queue counts and the fix.
//!
//! Fires only for users who WERE logged in (stored credentials whose refresh
//! token has expired) — never for users who never logged in. Gated on an
//! interactive stdout so it can't pollute scripts or piped output, emitted at
//! most once per process, and rate-limited to once per 24 hours across
//! processes via a timestamp file.

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
}

/// True when the daemon recently reported auth-blocked sync attempts.
fn sync_auth_blocked_recently() -> bool {
    let Ok(raw) = std::fs::read_to_string(sync_blocked_stamp_path()) else {
        return false;
    };
    let Ok(at) = raw.trim().parse::<i64>() else {
        return false;
    };
    unix_now() - at < SYNC_BLOCKED_FRESH_SECS
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

/// Count of locally queued items waiting for cloud upload: (notes, transcripts).
/// Best-effort — returns zeros when the databases are unavailable.
fn pending_upload_counts() -> (i64, i64) {
    let notes = crate::notes::db::NotesDatabase::global()
        .ok()
        .and_then(|db| db.lock().ok().map(|lock| lock.count_pending().unwrap_or(0)))
        .unwrap_or(0);
    let transcripts = crate::authorship::internal_db::InternalDatabase::global()
        .ok()
        .and_then(|db| {
            db.lock()
                .ok()
                .map(|lock| lock.count_pending_cas().unwrap_or(0))
        })
        .unwrap_or(0);
    (notes, transcripts)
}

/// Print a reminder to stderr when the user's login has expired.
///
/// Call from interactive command paths (autter subcommands and the git proxy).
/// All gates are cheap and short-circuit: terminal check, per-process flag,
/// 24-hour stamp file, then the credentials read.
pub fn maybe_warn_logged_out() {
    if !std::io::stdout().is_terminal() {
        return;
    }
    if NOTICE_EMITTED.load(Ordering::Relaxed) || recently_notified() {
        return;
    }

    // Only warn when the user WAS logged in (stored credentials exist) and
    // the session no longer works — either the refresh token is past its
    // expiry, or the daemon reports that authenticated sync is failing (a
    // token that is valid by timestamp but rejected by the server).
    let Ok(Some(creds)) = CredentialStore::new().load() else {
        return;
    };
    if !creds.is_refresh_token_expired() && !sync_auth_blocked_recently() {
        return;
    }

    if NOTICE_EMITTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    record_notified();

    let (notes, transcripts) = pending_upload_counts();

    eprintln!();
    eprintln!("\x1b[1;33m⚠ You've been logged out of autter — cloud sync is paused.\x1b[0m");
    if notes > 0 || transcripts > 0 {
        eprintln!(
            "\x1b[1;33m  {} authorship note{} and {} transcript{} are stored locally and will upload once you're back in.\x1b[0m",
            notes,
            if notes == 1 { "" } else { "s" },
            transcripts,
            if transcripts == 1 { "" } else { "s" },
        );
    }
    eprintln!("\x1b[1;33m  Run \x1b[1;36mautter login\x1b[0m\x1b[1;33m to reconnect.\x1b[0m");
    eprintln!();
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
