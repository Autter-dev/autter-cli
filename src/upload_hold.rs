//! Holding upload backlogs until the user explicitly consents.
//!
//! Some queued upload data must not reach the Autter platform without a fresh
//! "yes" from the user:
//!
//! - **Legacy backlog.** autter 2.2.0 and earlier queued data that should never
//!   have been queued: transcripts (CAS) with local prompt storage and for repos
//!   excluded from prompt sharing, and metrics/file-change counts in local mode.
//!   The queues record neither the mode nor (for transcripts) the repository an
//!   item was queued under, so those items cannot be told apart from
//!   legitimate ones. A one-time migration ([`ensure_legacy_migration`], gated
//!   by a version marker) therefore *holds* every item queued before it ran.
//!   Nothing is deleted: the transcript queue doubles as the local store that
//!   `autter show-prompt`/`blame` read, so purging it would lose local data.
//! - **Backlog that predates connecting.** `autter onboard --connect` holds the
//!   current backlog before signing in, so the background service cannot drain
//!   it while onboarding asks what to do with it.
//!
//! Provenance going forward needs no new columns: since local mode stopped
//! queueing upload data and excluded repos stopped queueing transcripts, every
//! item queued after the migration was queued in connected mode with sharing
//! allowed. "Queued before the migration / before connecting" is exactly what
//! the hold records.
//!
//! A held item stays on disk but is never dequeued:
//! - notes, commit summaries, transcripts and file-change rows get
//!   `next_retry_at = HELD_RETRY_AT`; every dequeue requires
//!   `next_retry_at <= now`, so even an older autter binary never drains them;
//! - metrics events move to a `metrics_held` side table that no flush reads.
//!
//! [`release_all`] (`autter sync import`, or "Import" during onboarding) makes
//! them eligible again; `autter sync purge` deletes them.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `next_retry_at` value that marks a queue row as held. No `now` ever
/// reaches it, so retry-time filters skip held rows.
pub const HELD_RETRY_AT: i64 = i64::MAX;

/// Bump to re-run [`ensure_legacy_migration`] on every machine.
pub const LEGACY_MIGRATION_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct HoldState {
    /// Last [`LEGACY_MIGRATION_VERSION`] applied on this machine.
    #[serde(default)]
    legacy_migration: u32,
}

/// Per-queue item counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct QueueCounts {
    pub metrics: i64,
    pub notes: i64,
    pub commit_summaries: i64,
    pub transcripts: i64,
    pub file_changes: i64,
}

impl QueueCounts {
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

fn state_path() -> Option<PathBuf> {
    crate::config::internal_dir_path().map(|dir| dir.join("upload_hold.json"))
}

fn load_state() -> HoldState {
    // A missing or unreadable marker means "not migrated": re-running the
    // migration only holds more, which is the safe direction.
    state_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_state(state: &HoldState) -> Result<(), String> {
    let path = state_path().ok_or_else(|| "could not locate ~/.autter/internal".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Hold everything currently queued for upload, across every queue.
pub fn hold_all() -> Result<QueueCounts, String> {
    let mut held = QueueCounts::default();
    let metrics = crate::metrics::db::MetricsDatabase::global().map_err(|e| e.to_string())?;
    held.metrics = lock(metrics)?.hold_all().map_err(|e| e.to_string())? as i64;

    let notes = crate::notes::db::NotesDatabase::global().map_err(|e| e.to_string())?;
    let (n, s) = lock(notes)?
        .hold_pending_uploads()
        .map_err(|e| e.to_string())?;
    held.notes = n as i64;
    held.commit_summaries = s as i64;

    let internal =
        crate::authorship::internal_db::InternalDatabase::global().map_err(|e| e.to_string())?;
    held.transcripts = lock(internal)?
        .hold_pending_cas()
        .map_err(|e| e.to_string())? as i64;

    let file_changes =
        crate::file_changes::FileChangesDatabase::global().map_err(|e| e.to_string())?;
    held.file_changes = lock(file_changes)?
        .hold_pending()
        .map_err(|e| e.to_string())? as i64;
    Ok(held)
}

/// Make every held item eligible for upload again (the user consented).
pub fn release_all() -> Result<QueueCounts, String> {
    let mut released = QueueCounts::default();
    let metrics = crate::metrics::db::MetricsDatabase::global().map_err(|e| e.to_string())?;
    released.metrics = lock(metrics)?.release_held().map_err(|e| e.to_string())? as i64;

    let notes = crate::notes::db::NotesDatabase::global().map_err(|e| e.to_string())?;
    let (n, s) = lock(notes)?
        .release_held_uploads()
        .map_err(|e| e.to_string())?;
    released.notes = n as i64;
    released.commit_summaries = s as i64;

    let internal =
        crate::authorship::internal_db::InternalDatabase::global().map_err(|e| e.to_string())?;
    released.transcripts = lock(internal)?
        .release_held_cas()
        .map_err(|e| e.to_string())? as i64;

    let file_changes =
        crate::file_changes::FileChangesDatabase::global().map_err(|e| e.to_string())?;
    released.file_changes = lock(file_changes)?
        .release_held()
        .map_err(|e| e.to_string())? as i64;
    Ok(released)
}

/// Items currently held. Best-effort: an unavailable database counts as zero.
pub fn held_counts() -> QueueCounts {
    let mut held = QueueCounts::default();
    if let Ok(db) = crate::metrics::db::MetricsDatabase::global()
        && let Ok(db) = db.lock()
    {
        held.metrics = db.count_held().unwrap_or(0) as i64;
    }
    if let Ok(db) = crate::notes::db::NotesDatabase::global()
        && let Ok(db) = db.lock()
        && let Ok((n, s)) = db.count_held_uploads()
    {
        held.notes = n;
        held.commit_summaries = s;
    }
    if let Ok(db) = crate::authorship::internal_db::InternalDatabase::global()
        && let Ok(db) = db.lock()
    {
        held.transcripts = db.count_held_cas().unwrap_or(0);
    }
    if let Ok(db) = crate::file_changes::FileChangesDatabase::global()
        && let Ok(db) = db.lock()
    {
        held.file_changes = db.count_held().unwrap_or(0);
    }
    held
}

/// One-time migration: hold every item queued by autter 2.2.0 or earlier.
///
/// Must run before anything drains a queue. Returns `Ok(true)` when the
/// queues are safe to drain (migration already applied, or applied now), and
/// `Err` when it could not be applied, in which case callers must not drain.
pub fn ensure_legacy_migration() -> Result<bool, String> {
    let mut state = load_state();
    if state.legacy_migration >= LEGACY_MIGRATION_VERSION {
        return Ok(true);
    }
    let held = hold_all()?;
    if held.total() > 0 {
        tracing::info!(
            held = %held.summary(),
            "upload hold: holding backlog queued by an earlier autter version until the user consents"
        );
    }
    state.legacy_migration = LEGACY_MIGRATION_VERSION;
    save_state(&state)?;
    Ok(true)
}

/// The commands that resolve a held backlog, for user-facing messages.
pub const IMPORT_COMMAND: &str = "autter sync import --yes";
pub const PURGE_COMMAND: &str = "autter sync purge --force";

fn lock<T>(m: &'static std::sync::Mutex<T>) -> Result<std::sync::MutexGuard<'static, T>, String> {
    m.lock().map_err(|_| "database lock poisoned".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_state_round_trips_and_defaults_to_unmigrated() {
        let state: HoldState = serde_json::from_str("{}").unwrap();
        assert_eq!(state.legacy_migration, 0);
        let state = HoldState {
            legacy_migration: LEGACY_MIGRATION_VERSION,
        };
        let back: HoldState = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn queue_counts_total() {
        let counts = QueueCounts {
            metrics: 1,
            notes: 2,
            commit_summaries: 3,
            transcripts: 4,
            file_changes: 5,
        };
        assert_eq!(counts.total(), 15);
    }
}
