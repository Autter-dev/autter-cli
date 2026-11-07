//! Backfill `refs/notes/ai` from the notes database.
//!
//! Under the `http` notes backend an authorship note is written only to the
//! notes database (`~/.autter/internal/notes-db`), never to git. Connected mode
//! used to run on that backend, so commits made then have no local git note.
//! Once the machine reads its backend as `both` (connected mode today) or
//! `git_notes` (local mode), those commits look unattributed to `autter blame`,
//! `autter log --plain` and anything else that reads git refs.
//!
//! The backfill copies those notes into `refs/notes/ai`:
//! - only for commits reachable in the current repository;
//! - never over a note that already exists locally (local notes win);
//! - idempotently, so a second run writes nothing.
//!
//! It runs once per repository automatically on the first note write under a
//! backend that writes git notes (guarded by a marker file in `.git/ai/`), and
//! on demand through `autter notes backfill`.

use crate::error::AutterError;
use crate::git::refs::{notes_add_batch, notes_for_commits};
use crate::git::repository::{Repository, exec_git};
use crate::notes::db::NotesDatabase;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Marker written to `<common git dir>/ai/` after the automatic backfill has run
/// for a repository. Delete it (or run `autter notes backfill`) to run again.
pub const AUTO_BACKFILL_MARKER: &str = "notes_backfill_v1";

/// How many commits to look up or write per git invocation.
const CHUNK: usize = 500;

/// Copy notes that exist only in the global notes database into
/// `refs/notes/ai` for this repository. Returns how many notes were written.
pub fn backfill_local_notes(repo: &Repository) -> Result<usize, AutterError> {
    let db = NotesDatabase::global()?;
    backfill_with(
        repo,
        || {
            let guard = db
                .lock()
                .map_err(|e| AutterError::Generic(format!("notes-db lock: {}", e)))?;
            guard.all_note_shas()
        },
        |shas| {
            let guard = db
                .lock()
                .map_err(|e| AutterError::Generic(format!("notes-db lock: {}", e)))?;
            guard.get_notes(shas)
        },
    )
}

/// Same as [`backfill_local_notes`], reading from an explicit database handle.
pub fn backfill_local_notes_from_db(
    repo: &Repository,
    db: &NotesDatabase,
) -> Result<usize, AutterError> {
    backfill_with(repo, || db.all_note_shas(), |shas| db.get_notes(shas))
}

/// Run the backfill once per repository, after a note write under a backend
/// that writes git notes. Never fails the caller: errors are logged and the
/// marker is left unwritten so the next write tries again.
pub fn maybe_auto_backfill(repo: &Repository) {
    let marker = auto_backfill_marker_path(repo);
    if marker.exists() {
        return;
    }
    // Machines that never stored a note in the database have nothing to copy;
    // don't create the database file or a marker for them.
    if !NotesDatabase::exists_on_disk() {
        return;
    }
    let has_rows = NotesDatabase::global()
        .ok()
        .and_then(|db| db.lock().ok().and_then(|g| g.has_any_notes().ok()))
        .unwrap_or(false);
    if !has_rows {
        return;
    }

    match backfill_local_notes(repo) {
        Ok(written) => {
            if written > 0 {
                tracing::info!(
                    written,
                    "backfilled authorship notes from notes-db into refs/notes/ai"
                );
            }
            if let Some(parent) = marker.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&marker, format!("{}\n", written)) {
                tracing::warn!(%e, "failed to write notes backfill marker");
            }
        }
        Err(e) => {
            tracing::warn!(%e, "automatic notes backfill failed; will retry on next write");
        }
    }
}

/// Path of the per-repository marker for the automatic backfill.
pub fn auto_backfill_marker_path(repo: &Repository) -> PathBuf {
    repo.common_dir().join("ai").join(AUTO_BACKFILL_MARKER)
}

fn backfill_with<L, G>(
    repo: &Repository,
    list_shas: L,
    mut get_notes: G,
) -> Result<usize, AutterError>
where
    L: FnOnce() -> Result<Vec<String>, AutterError>,
    G: FnMut(&[&str]) -> Result<HashMap<String, String>, AutterError>,
{
    let db_shas: Vec<String> = list_shas()?
        .into_iter()
        .filter(|sha| is_full_hex_oid(sha))
        .collect();
    if db_shas.is_empty() {
        return Ok(0);
    }

    // Only commits that belong to this repository. The notes database is
    // shared by every repository on the machine.
    let reachable = reachable_commits(repo)?;
    let candidates: Vec<String> = db_shas
        .into_iter()
        .filter(|sha| reachable.contains(sha))
        .collect();
    if candidates.is_empty() {
        return Ok(0);
    }

    let mut written = 0usize;
    for chunk in candidates.chunks(CHUNK) {
        // Local notes win: skip any commit that already has one.
        let existing = notes_for_commits(repo, chunk)?;
        let missing: Vec<&str> = chunk
            .iter()
            .filter(|sha| !existing.contains_key(*sha))
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            continue;
        }

        let contents = get_notes(&missing)?;
        let mut entries: Vec<(String, String)> = missing
            .iter()
            .filter_map(|sha| {
                contents
                    .get(*sha)
                    .filter(|content| !content.trim().is_empty())
                    .map(|content| (sha.to_string(), content.clone()))
            })
            .collect();
        if entries.is_empty() {
            continue;
        }
        entries.sort();
        notes_add_batch(repo, &entries)?;
        written += entries.len();
    }

    Ok(written)
}

fn reachable_commits(repo: &Repository) -> Result<HashSet<String>, AutterError> {
    let mut args = repo.global_args_for_exec();
    args.push("rev-list".to_string());
    args.push("--all".to_string());
    let output = exec_git(&args)?;
    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn is_full_hex_oid(sha: &str) -> bool {
    (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::find_repository_in_path;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn commit(dir: &std::path::Path, name: &str) -> String {
        std::fs::write(dir.join(name), name).unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", name]);
        git(dir, &["rev-parse", "HEAD"])
    }

    fn note_json(tag: &str) -> String {
        format!(
            "{{\"schema_version\":\"authorship/3.0.0\",\"tag\":\"{}\"}}",
            tag
        )
    }

    #[test]
    fn backfill_writes_db_only_notes_and_keeps_local_ones() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        let a = commit(&dir, "a.txt");
        let b = commit(&dir, "b.txt");
        let c = commit(&dir, "c.txt");

        let repo = find_repository_in_path(dir.to_str().unwrap()).unwrap();
        // `c` already has a local note; the database copy must not replace it.
        crate::git::refs::notes_add(&repo, &c, "local-wins").unwrap();

        let mut db = NotesDatabase::open_at_path(&tmp.path().join("notes-db")).unwrap();
        db.cache_synced_notes(&[(a.clone(), note_json("a"))])
            .unwrap();
        db.upsert_note_with_repo(&b, &note_json("b"), None).unwrap();
        db.upsert_note_with_repo(&c, &note_json("c"), None).unwrap();
        // A commit from some other repository on this machine.
        db.upsert_note_with_repo(&"f".repeat(40), &note_json("other"), None)
            .unwrap();

        let written = backfill_local_notes_from_db(&repo, &db).unwrap();
        assert_eq!(written, 2, "only a and b lack local notes");

        let notes = notes_for_commits(&repo, &[a.clone(), b.clone(), c.clone()]).unwrap();
        assert_eq!(
            notes.get(&a).map(|s| s.trim()),
            Some(note_json("a").as_str())
        );
        assert_eq!(
            notes.get(&b).map(|s| s.trim()),
            Some(note_json("b").as_str())
        );
        assert_eq!(notes.get(&c).map(|s| s.trim()), Some("local-wins"));

        // Idempotent: a second run writes nothing.
        assert_eq!(backfill_local_notes_from_db(&repo, &db).unwrap(), 0);
    }

    #[test]
    fn backfill_is_a_no_op_with_an_empty_database() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        commit(&dir, "a.txt");

        let repo = find_repository_in_path(dir.to_str().unwrap()).unwrap();
        let db = NotesDatabase::open_at_path(&tmp.path().join("notes-db")).unwrap();
        assert_eq!(backfill_local_notes_from_db(&repo, &db).unwrap(), 0);
        let list = git(&dir, &["notes", "--ref=ai", "list"]);
        assert!(list.is_empty(), "no notes expected, got {list:?}");
    }

    #[test]
    fn is_full_hex_oid_rejects_partial_and_non_hex() {
        assert!(is_full_hex_oid(&"a".repeat(40)));
        assert!(is_full_hex_oid(&"0".repeat(64)));
        assert!(!is_full_hex_oid("abc123"));
        assert!(!is_full_hex_oid(&"g".repeat(40)));
    }
}
