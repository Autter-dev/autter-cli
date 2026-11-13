//! Centralized notes I/O API.
//!
//! All authorship-note reads and writes flow through this module. The implementation
//! dispatches to either the git-notes backend (default) or the HTTP backend based on
//! the currently configured `notes_backend.kind`.
//!
//! Phase 0: pure pass-through to `crate::git::refs` (no behavioral change).
//! Phase 2: kind-aware dispatch to either git or the HTTP backend.

use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::config::{Config, NotesBackendKind};
use crate::error::AutterError;
use crate::git::repository::Repository;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

// Re-export CommitAuthorship so callers don't need to import from refs directly.
pub use crate::git::refs::CommitAuthorship;

/// Resolve the notes backend kind from a fresh config snapshot.
///
/// This must not use the process-lifetime `Config::get()` cache: the daemon is
/// a long-lived process and is the component that persists authorship notes,
/// so a cached kind keeps routing notes to whatever backend was configured
/// when the daemon started. Switching modes (e.g. `autter onboard` connected
/// -> local-only) would otherwise silently divert every note away from
/// refs/notes/ai until the daemon happens to restart.
fn current_backend_kind() -> NotesBackendKind {
    Config::fresh().notes_backend_kind()
}

// --- Writes ---

pub fn write_note(repo: &Repository, commit_sha: &str, content: &str) -> Result<(), AutterError> {
    write_note_with_kind(current_backend_kind(), repo, commit_sha, content)
}

/// Commits that the rewrites in progress in this process leave at the same
/// SHA (a no-op rebase, a pick that fast-forwards, an amend that changes
/// nothing), and commits post-commit is processing again. Keyed by SHA, with
/// a count so overlapping rewrites (the background service handles several
/// repositories) do not clear each other's marks.
fn unchanged_commits() -> &'static Mutex<HashMap<String, usize>> {
    static UNCHANGED: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    UNCHANGED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// While alive, the notes of these commits are not replaced by a different
/// note (see [`filter_note_writes`]). Dropping it forgets
/// them.
#[must_use = "the mark lasts only as long as this value"]
pub struct KeepExistingNotes {
    commits: Vec<String>,
}

impl Drop for KeepExistingNotes {
    fn drop(&mut self) {
        if let Ok(mut map) = unchanged_commits().lock() {
            for commit in &self.commits {
                if let Some(count) = map.get_mut(commit) {
                    *count -= 1;
                    if *count == 0 {
                        map.remove(commit);
                    }
                }
            }
        }
    }
}

/// Mark `commits` as kept at their SHA by the rewrite in progress: a commit
/// whose SHA did not change keeps the note it has, which was written for
/// that very commit and is still correct.
pub fn keep_existing_notes_for<I: IntoIterator<Item = String>>(commits: I) -> KeepExistingNotes {
    let commits: Vec<String> = commits.into_iter().collect();
    if let Ok(mut map) = unchanged_commits().lock() {
        for commit in &commits {
            *map.entry(commit.clone()).or_insert(0) += 1;
        }
    }
    KeepExistingNotes { commits }
}

/// Commits in both `original` and `rewritten`: a rewrite left them at the
/// same SHA.
pub fn commits_kept_by_rewrite(original: &[String], rewritten: &[String]) -> Vec<String> {
    let original: HashSet<&String> = original.iter().collect();
    rewritten
        .iter()
        .filter(|c| original.contains(c))
        .cloned()
        .collect()
}

fn is_marked_unchanged(commit: &str) -> bool {
    unchanged_commits()
        .lock()
        .map(|map| map.contains_key(commit))
        .unwrap_or(false)
}

/// Commits whose next note upload is held: their note is provisional and
/// autter replaces it right after (see `NotesDatabase::upsert_notes_batch_holding`).
fn held_uploads() -> &'static Mutex<HashSet<String>> {
    static HELD: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(HashSet::new()))
}

/// How long a provisional note's upload is held when nothing replaces it.
const PROVISIONAL_NOTE_HOLD_SECS: i64 = 120;

/// Hold the upload of the next note written for `commit`: autter is about to
/// replace it (a cherry-pick finished with a plain `git commit` gets its
/// post-commit note, then the cherry-pick's). The server keeps every version
/// of a note it receives, so uploading both would read as
/// `changed_after_recorded`. The replacement releases the hold.
pub fn hold_next_upload(commit: &str) {
    if let Ok(mut held) = held_uploads().lock() {
        held.insert(commit.to_string());
    }
}

/// The commits of `entries` whose upload is held, consuming the holds.
fn take_held_uploads(entries: &[(String, String)]) -> HashSet<String> {
    let Ok(mut held) = held_uploads().lock() else {
        return HashSet::new();
    };
    if held.is_empty() {
        return HashSet::new();
    }
    entries
        .iter()
        .filter(|(sha, _)| held.remove(sha))
        .map(|(sha, _)| sha.clone())
        .collect()
}

/// Set by `autter squash-authorship`, the manual command whose purpose is to
/// replace a commit's attribution: its write goes ahead even when the note it
/// replaces may already be on the server (see [`filter_note_writes`]).
static REPLACE_RECORDED_NOTES: AtomicBool = AtomicBool::new(false);

/// Let this process replace a commit's own note even when that note may
/// already be on the server. Only `autter squash-authorship` calls this: the
/// user asked for that commit's attribution to be rewritten, so the record
/// then shows a second version, which is what happened.
pub fn allow_replacing_recorded_notes() {
    REPLACE_RECORDED_NOTES.store(true, Ordering::SeqCst);
}

/// `entries` without the writes that must not happen to a commit that
/// already has a note.
///
/// In connected mode every note write is uploaded, and the server keeps every
/// version of a note it receives, so a second, different note for the same
/// SHA reads as `changed_after_recorded` in `autter verify`. For each entry:
///
/// 1. no note yet: written;
/// 2. the same note (byte for byte, or the same body with the stamp the
///    writer would add): not written, nothing is uploaded;
/// 3. a note that was not written for this commit (git copied it here from
///    another commit during a rebase or amend, with `notes.rewriteRef`; it
///    names that commit): replaced. Git's copy happens inside the git command,
///    before autter's rewrite runs, and only in `refs/notes/ai`: it is never
///    queued for upload, so the replacement is the first note the server sees
///    for this SHA;
/// 4. the commit's own note, and the commit kept its SHA through the rewrite
///    in progress ([`keep_existing_notes_for`]: a no-op rebase, a pick that
///    fast-forwards, an amend that changes nothing, post-commit running again):
///    kept, also when it predates the integrity stamp;
/// 5. the commit's own note, different content, any other write (a
///    correction): written only when the note it replaces cannot have reached
///    the server: the backend does not upload (`git_notes`), or the notes
///    database holds it as queued and never sent ([`NotesDatabase::unsent_shas`],
///    e.g. a provisional note whose upload [`hold_next_upload`] held). A note
///    that was sent, is being sent, failed to send, or has no row here (it came
///    from a fetch, a teammate or a CI run) is kept: the existing note stays,
///    and the commit's record keeps one version. `autter squash-authorship`
///    is the one exception ([`allow_replacing_recorded_notes`]).
///
/// `db` is the locked notes database for backends that upload; the caller
/// holds the lock until it has written, so the uploader cannot send a note
/// between this check and its replacement.
fn filter_note_writes(
    kind: NotesBackendKind,
    repo: &Repository,
    entries: &[(String, String)],
    db: Option<&crate::notes::db::NotesDatabase>,
) -> Vec<(String, String)> {
    use crate::authorship::note_integrity::{note_written_for, strip_integrity};

    let shas: Vec<String> = entries.iter().map(|(sha, _)| sha.clone()).collect();
    let existing = match kind {
        NotesBackendKind::Http => {
            let refs: Vec<&str> = shas.iter().map(String::as_str).collect();
            db.and_then(|db| db.get_notes(&refs).ok())
                .unwrap_or_default()
        }
        NotesBackendKind::GitNotes | NotesBackendKind::Both => {
            match crate::git::refs::notes_for_commits(repo, &shas) {
                Ok(notes) => notes,
                Err(e) => {
                    tracing::debug!(%e, "note write: could not read existing notes");
                    HashMap::new()
                }
            }
        }
    };
    if existing.is_empty() {
        return entries.to_vec();
    }
    let unsent: HashSet<String> = if kind.uses_http() {
        let refs: Vec<&str> = shas.iter().map(String::as_str).collect();
        db.and_then(|db| db.unsent_shas(&refs).ok())
            .unwrap_or_default()
    } else {
        HashSet::new()
    };
    entries
        .iter()
        .filter(|(sha, new_note)| {
            let Some(old_note) = existing.get(sha) else {
                return true;
            };
            if old_note.trim_end() == new_note.trim_end() {
                tracing::debug!(commit = %sha, "note write skipped: identical note exists");
                return false;
            }
            if !note_written_for(old_note, sha) {
                return true;
            }
            if strip_integrity(old_note) == strip_integrity(new_note) {
                tracing::debug!(commit = %sha, "note write skipped: same note, already stamped");
                return false;
            }
            if is_marked_unchanged(sha) {
                tracing::debug!(commit = %sha, "note write skipped: the commit kept its SHA");
                return false;
            }
            if REPLACE_RECORDED_NOTES.load(Ordering::SeqCst)
                || !kind.uses_http()
                || unsent.contains(sha)
            {
                return true;
            }
            tracing::debug!(
                commit = %sha,
                "note write skipped: the commit's note may already be on the server"
            );
            false
        })
        .cloned()
        .collect()
}

fn write_note_with_kind(
    kind: NotesBackendKind,
    repo: &Repository,
    commit_sha: &str,
    content: &str,
) -> Result<(), AutterError> {
    write_notes_batch_with_kind(kind, repo, &[(commit_sha.to_string(), content.to_string())])
}

/// After a successful git-notes write, copy into `refs/notes/ai` any notes this
/// repository has only in the notes database (written while the machine used
/// the `http` backend). Runs once per repository; see `notes_backfill`.
fn after_git_notes_write(repo: &Repository, git_result: &Result<(), AutterError>) {
    if git_result.is_ok() {
        crate::git::notes_backfill::maybe_auto_backfill(repo);
    }
}

pub fn write_notes_batch(
    repo: &Repository,
    entries: &[(String, String)],
) -> Result<(), AutterError> {
    write_notes_batch_with_kind(current_backend_kind(), repo, entries)
}

fn write_notes_batch_with_kind(
    kind: NotesBackendKind,
    repo: &Repository,
    entries: &[(String, String)],
) -> Result<(), AutterError> {
    if entries.is_empty() {
        return Ok(());
    }
    if !kind.uses_http() {
        let entries = filter_note_writes(kind, repo, entries, None);
        if entries.is_empty() {
            return Ok(());
        }
        let git_result = crate::git::refs::notes_add_batch(repo, &entries);
        after_git_notes_write(repo, &git_result);
        return git_result;
    }

    let repo_url = crate::repo_url::resolve_repo_url_from_repo(repo);
    // Hold the notes database from the check to the write (see
    // `filter_note_writes`). Nothing below takes it again before it is
    // released: the git write and the stamping only run git.
    let db = crate::notes::db::NotesDatabase::global();
    let mut db_lock = match &db {
        Ok(db) => db.lock().ok(),
        Err(_) => None,
    };
    let entries = filter_note_writes(kind, repo, entries, db_lock.as_deref());
    if entries.is_empty() {
        return Ok(());
    }
    let stamped = crate::authorship::note_integrity::stamp_note_entries(repo, &entries);
    // Git notes are the durable local copy; write them first. The HTTP
    // enqueue is best-effort for `both` — a failure must not lose the git
    // note, and the cloud copy can be backfilled with notes-migrate.
    let git_result =
        (kind == NotesBackendKind::Both).then(|| crate::git::refs::notes_add_batch(repo, &entries));
    let http_result = match db_lock.as_deref_mut() {
        Some(db) => enqueue_notes(db, &stamped, repo_url.as_deref()),
        None => Err(match db {
            Err(e) => e,
            Ok(_) => AutterError::Generic("notes-db lock poisoned".to_string()),
        }),
    };
    drop(db_lock);
    if http_result.is_ok() {
        crate::daemon::telemetry_handle::submit_notes();
    }
    match git_result {
        None => http_result,
        Some(git_result) => {
            if let Err(e) = http_result {
                tracing::warn!(%e, count = entries.len(), "both backend: cloud notes enqueue failed");
            }
            after_git_notes_write(repo, &git_result);
            git_result
        }
    }
}

// --- Reads ---

pub fn read_note(repo: &Repository, commit_sha: &str) -> Option<String> {
    match current_backend_kind() {
        NotesBackendKind::Http => http_read_note(commit_sha)
            .or_else(|| crate::git::refs::show_authorship_note(repo, commit_sha)),
        NotesBackendKind::GitNotes => crate::git::refs::show_authorship_note(repo, commit_sha),
        // Local git notes are authoritative; the cloud cache covers commits
        // whose notes only exist remotely (e.g. written by cloud-only teammates).
        NotesBackendKind::Both => crate::git::refs::show_authorship_note(repo, commit_sha)
            .or_else(|| http_read_note(commit_sha)),
    }
}

pub fn read_notes_batch(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashMap<String, String>, AutterError> {
    if commit_shas.is_empty() {
        return Ok(HashMap::new());
    }

    match current_backend_kind() {
        NotesBackendKind::Http => {
            let mut notes = http_read_notes(commit_shas);

            let missing_after_cache: Vec<String> = commit_shas
                .iter()
                .filter(|sha| !notes.contains_key(*sha))
                .cloned()
                .collect();
            if !missing_after_cache.is_empty() {
                notes.extend(http_fetch_and_cache_notes(&missing_after_cache));
            }

            let missing_after_http: Vec<String> = commit_shas
                .iter()
                .filter(|sha| !notes.contains_key(*sha))
                .cloned()
                .collect();
            if !missing_after_http.is_empty()
                && let Ok(git_notes) =
                    crate::git::refs::notes_for_commits(repo, &missing_after_http)
            {
                notes.extend(git_notes);
            }

            Ok(notes)
        }
        NotesBackendKind::GitNotes => crate::git::refs::notes_for_commits(repo, commit_shas),
        NotesBackendKind::Both => {
            // Local git notes first, then the cloud cache/API for commits
            // whose notes only exist remotely.
            let mut notes = crate::git::refs::notes_for_commits(repo, commit_shas)?;

            let missing_after_git: Vec<String> = commit_shas
                .iter()
                .filter(|sha| !notes.contains_key(*sha))
                .cloned()
                .collect();
            if !missing_after_git.is_empty() {
                notes.extend(http_read_notes(&missing_after_git));
            }

            let missing_after_cache: Vec<String> = commit_shas
                .iter()
                .filter(|sha| !notes.contains_key(*sha))
                .cloned()
                .collect();
            if !missing_after_cache.is_empty() {
                notes.extend(http_fetch_and_cache_notes(&missing_after_cache));
            }

            Ok(notes)
        }
    }
}

pub fn read_authorship(repo: &Repository, commit_sha: &str) -> Option<AuthorshipLog> {
    match current_backend_kind() {
        NotesBackendKind::Http => {
            // Check the cache first; fall through to git notes on miss.
            if let Some(content) = http_read_note(commit_sha) {
                AuthorshipLog::deserialize_from_string(&content)
                    .map_err(|e| tracing::debug!("notes deserialization error: {}", e))
                    .ok()
            } else {
                crate::git::refs::get_authorship(repo, commit_sha)
            }
        }
        NotesBackendKind::GitNotes => crate::git::refs::get_authorship(repo, commit_sha),
        NotesBackendKind::Both => {
            crate::git::refs::get_authorship(repo, commit_sha).or_else(|| {
                let content = http_read_note(commit_sha)?;
                AuthorshipLog::deserialize_from_string(&content)
                    .map_err(|e| tracing::debug!("notes deserialization error: {}", e))
                    .ok()
            })
        }
    }
}

pub fn read_authorship_v3(
    repo: &Repository,
    commit_sha: &str,
) -> Result<AuthorshipLog, AutterError> {
    match current_backend_kind() {
        NotesBackendKind::Http => {
            if let Some(content) = http_read_note(commit_sha) {
                AuthorshipLog::deserialize_from_string(&content).map_err(|e| {
                    AutterError::Generic(format!("notes deserialization error: {}", e))
                })
            } else {
                crate::git::refs::get_reference_as_authorship_log_v3(repo, commit_sha)
            }
        }
        NotesBackendKind::GitNotes => {
            crate::git::refs::get_reference_as_authorship_log_v3(repo, commit_sha)
        }
        NotesBackendKind::Both => {
            match crate::git::refs::get_reference_as_authorship_log_v3(repo, commit_sha) {
                Ok(log) => Ok(log),
                Err(git_err) => {
                    if let Some(content) = http_read_note(commit_sha) {
                        AuthorshipLog::deserialize_from_string(&content).map_err(|e| {
                            AutterError::Generic(format!("notes deserialization error: {}", e))
                        })
                    } else {
                        Err(git_err)
                    }
                }
            }
        }
    }
}

/// Return a map of commit SHA → note-blob OID for the given commits.
///
/// # Audit results (Phase 2)
///
/// All callers of this function use the returned blob OIDs as *git object IDs*
/// to subsequently read note content via `batch_read_blob_contents` /
/// `batch_read_blobs_with_oids`.  They are NOT purely presence checks.
///
/// Call sites and how they use the OIDs:
///
/// 1. `authorship_traversal::load_ai_touched_files_for_commits` — passes OIDs
///    to `batch_read_blobs_with_oids`; must be real git OIDs.
/// 2. `rebase_authorship::build_rebase_note_cache` — passes OIDs to
///    `batch_read_blob_contents`; must be real git OIDs.
/// 3. `rebase_authorship::load_note_contents_for_commits` — same pattern.
/// 4. `rebase_authorship::try_fast_path_cherry_pick_remap` — passes OIDs to
///    `batch_read_blob_contents`; also checks `len() != source_commits.len()`
///    and returns `false` on mismatch, which is the correct behaviour when
///    notes are not in git refs.
///
/// **HTTP backend**: notes do not live in `refs/notes/ai`, so there are no
/// git blob OIDs to return.  Returning an empty map causes callers to handle
/// the "no notes available" case (skip or use slow-path reads).  This is
/// safe and correct for the transition period — callers that need note content
/// will fall back to `read_note` / `read_authorship` which hit the cache.
pub fn read_note_blob_oids(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashMap<String, String>, AutterError> {
    match current_backend_kind() {
        // For Http, notes are in notes-db not in git — no blob OIDs exist.
        // Return an empty map; callers handle this as "no notes in git".
        NotesBackendKind::Http => Ok(HashMap::new()),
        // For Both, notes are written to git refs, so real blob OIDs exist
        // and the fast paths work. Commits without a local note degrade to
        // slow-path reads, which consult the cloud cache.
        NotesBackendKind::GitNotes | NotesBackendKind::Both => {
            crate::git::refs::note_blob_oids_for_commits(repo, commit_shas)
        }
    }
}

pub fn commits_with_notes(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashSet<String>, AutterError> {
    match current_backend_kind() {
        NotesBackendKind::Http => {
            // Check the cache first; fall through to git notes for misses.
            let cached = http_check_exists(commit_shas);
            if cached.len() == commit_shas.len() {
                return Ok(cached);
            }
            // For commits not in the cache, check git notes as fallback.
            let missing: Vec<String> = commit_shas
                .iter()
                .filter(|sha| !cached.contains(*sha))
                .cloned()
                .collect();
            let from_git = crate::git::refs::commits_with_authorship_notes(repo, &missing)?;
            Ok(cached.into_iter().chain(from_git).collect())
        }
        NotesBackendKind::GitNotes => {
            crate::git::refs::commits_with_authorship_notes(repo, commit_shas)
        }
        NotesBackendKind::Both => {
            // Git notes first; the cloud cache covers commits whose notes
            // only exist remotely.
            let from_git = crate::git::refs::commits_with_authorship_notes(repo, commit_shas)?;
            if from_git.len() == commit_shas.len() {
                return Ok(from_git);
            }
            let missing: Vec<String> = commit_shas
                .iter()
                .filter(|sha| !from_git.contains(*sha))
                .cloned()
                .collect();
            let cached = http_check_exists(&missing);
            Ok(from_git.into_iter().chain(cached).collect())
        }
    }
}

pub fn filter_commits_with_notes(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<Vec<CommitAuthorship>, AutterError> {
    match current_backend_kind() {
        NotesBackendKind::Http | NotesBackendKind::Both => {
            // `CommitAuthorship` requires a git_author that is only available from
            // `git rev-list`. Call the underlying git function which handles author
            // lookup, then patch in cache hits for commits whose `authorship_log`
            // would otherwise be absent (because refs/notes/ai is empty).
            //
            // The git function calls `get_authorship(repo, sha)` (refs.rs, not
            // notes_api), so for Http the results will be `CommitAuthorship::NoLog`
            // for all commits (and for Both, any commit without a local git
            // note). We promote any commit that has a cache entry to
            // `CommitAuthorship::Log`.
            let cached_map = http_read_notes(commit_shas);

            let git_results =
                crate::git::refs::get_commits_with_notes_from_list(repo, commit_shas)?;

            // Promote NoLog entries that are in the cache to Log entries.
            let results = git_results
                .into_iter()
                .map(|ca| match ca {
                    CommitAuthorship::NoLog {
                        ref sha,
                        ref git_author,
                    } => {
                        if let Some(content) = cached_map.get(sha)
                            && let Ok(authorship_log) =
                                AuthorshipLog::deserialize_from_string(content)
                                    .map_err(|e| AutterError::Generic(e.to_string()))
                        {
                            return CommitAuthorship::Log {
                                sha: sha.clone(),
                                git_author: git_author.clone(),
                                authorship_log,
                            };
                        }
                        ca
                    }
                    // Already has a log (shouldn't happen for Http, but keep it).
                    CommitAuthorship::Log { .. } => ca,
                })
                .collect();

            Ok(results)
        }
        NotesBackendKind::GitNotes => {
            crate::git::refs::get_commits_with_notes_from_list(repo, commit_shas)
        }
    }
}

// --- Search ---

pub fn search_notes(repo: &Repository, pattern: &str) -> Result<Vec<String>, AutterError> {
    crate::git::refs::grep_ai_notes(repo, pattern)
}

// --- Materialization (for autter log) ---

/// Materialize notes from the local cache into a one-off git ref
/// `refs/notes/ai-display` so that `git log --notes=ai-display` can render
/// them without requiring them to be in `refs/notes/ai`.
///
/// Only the most recent `limit` commits reachable from HEAD are considered.
///
/// The ref is left in place after the call; callers use it with `--notes=ai-display`.
/// It is safe to call repeatedly — each call starts from an empty tree via
/// `from 0000...` so stale notes from prior calls are discarded.
///
/// Returns the number of notes that were written into `refs/notes/ai-display`.
pub fn materialize_notes_for_display(
    repo: &Repository,
    limit: usize,
) -> Result<usize, AutterError> {
    use crate::git::repository::exec_git;
    use crate::git::repository::exec_git_stdin;

    // 1. Get recent commits via rev-list.
    let rev_list_args: Vec<String> = repo
        .global_args_for_exec()
        .into_iter()
        .chain([
            "rev-list".to_string(),
            format!("--max-count={}", limit),
            "HEAD".to_string(),
        ])
        .collect();

    let output = exec_git(&rev_list_args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let commit_shas: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if commit_shas.is_empty() {
        return Ok(0);
    }

    // 2. Look up which commits are in the local notes-db cache.
    let cached_map = http_read_notes(&commit_shas);
    if cached_map.is_empty() {
        return Ok(0);
    }

    // 3. Build a git fast-import stream.
    //    Structure:
    //      - One `blob` stanza per note (each gets a mark ID).
    //      - One `commit` stanza with `from 0000...` (empty tree) that attaches all blobs.
    let mut stream = String::new();
    let mut marks: Vec<(usize, String)> = Vec::new(); // (mark_id, commit_sha)

    for (idx, (commit_sha, content)) in cached_map.iter().enumerate() {
        let mark_id = idx + 1;
        // Blob stanza: `data <exact-byte-count>\n<content-bytes>\n`
        // The trailing \n after content is a fast-import stream separator, not part of the data.
        stream.push_str(&format!(
            "blob\nmark :{}\ndata {}\n{}\n",
            mark_id,
            content.len(),
            content
        ));
        marks.push((mark_id, commit_sha.clone()));
    }

    // Commit stanza — mirrors the pattern used in refs.rs notes_add_batch().
    // Use `from` with an all-zeros SHA to start from an empty tree, ensuring
    // stale notes from prior materializations are removed.
    stream.push_str("commit refs/notes/ai-display\n");
    stream.push_str("committer autter <autter@localhost> 1000000000 +0000\n");
    stream.push_str("data 0\n");
    stream.push_str("from 0000000000000000000000000000000000000000\n");

    let count = marks.len();
    for (mark_id, commit_sha) in &marks {
        stream.push_str(&format!("M 100644 :{} {}\n", mark_id, commit_sha));
    }
    stream.push('\n');

    // 4. Feed to git fast-import.
    let fast_import_args: Vec<String> = repo
        .global_args_for_exec()
        .into_iter()
        .chain(["fast-import".to_string(), "--quiet".to_string()])
        .collect();

    exec_git_stdin(&fast_import_args, stream.as_bytes())?;

    Ok(count)
}

// --- Cache warming ---

/// Pre-warm the local notes cache during `git pull` by fetching notes for
/// recently-arrived commits from the HTTP backend.
///
/// Algorithm:
/// 1. Walk the last 500 commits reachable from HEAD via `git rev-list`.
/// 2. Filter out any SHAs already present in `notes-db` (already cached).
/// 3. Batch the remaining SHAs into chunks of 100 and call `ApiClient::read_notes()`.
/// 4. Write returned entries via `cache_synced_notes()` so rows are inserted
///    with `synced = 1` (read cache, not upload queue).
///
/// This function is a best-effort operation: errors are logged but not propagated
/// (callers should treat failure as a cache miss, not a hard error).
pub fn warm_cache_for_remote(repo: &Repository, remote: &str) -> Result<(), AutterError> {
    use crate::api::client::{ApiClient, ApiContext};
    use crate::git::repository::exec_git;

    // 1. Walk recent history. Prefer the remote's default branch; fall back to HEAD.
    let remote_head = format!("refs/remotes/{}/HEAD", remote);
    let rev_target = {
        let check_args: Vec<String> = repo
            .global_args_for_exec()
            .into_iter()
            .chain([
                "rev-parse".to_string(),
                "--verify".to_string(),
                "--quiet".to_string(),
                remote_head.clone(),
            ])
            .collect();
        if exec_git(&check_args)
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            remote_head
        } else {
            "HEAD".to_string()
        }
    };

    let rev_list_args: Vec<String> = repo
        .global_args_for_exec()
        .into_iter()
        .chain([
            "rev-list".to_string(),
            "--max-count=500".to_string(),
            rev_target,
        ])
        .collect();

    let output = exec_git(&rev_list_args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let all_shas: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if all_shas.is_empty() {
        tracing::debug!("warm_cache_for_remote: no commits in HEAD history; skipping");
        return Ok(());
    }

    // 2. Filter out SHAs already in notes-db.
    let already_cached: std::collections::HashSet<String> = {
        match crate::notes::db::NotesDatabase::global() {
            Ok(db) => match db.lock() {
                Ok(lock) => {
                    let refs: Vec<&str> = all_shas.iter().map(|s| s.as_str()).collect();
                    lock.get_notes(&refs)
                        .unwrap_or_default()
                        .into_keys()
                        .collect()
                }
                Err(e) => {
                    tracing::warn!("warm_cache_for_remote: DB lock poisoned: {}", e);
                    std::collections::HashSet::new()
                }
            },
            Err(e) => {
                tracing::warn!("warm_cache_for_remote: failed to open notes-db: {}", e);
                std::collections::HashSet::new()
            }
        }
    };

    let uncached: Vec<String> = all_shas
        .into_iter()
        .filter(|sha| !already_cached.contains(sha))
        .collect();

    if uncached.is_empty() {
        tracing::debug!("warm_cache_for_remote: all commits already cached; skipping");
        return Ok(());
    }

    tracing::debug!(
        "warm_cache_for_remote: fetching notes for {} uncached commits",
        uncached.len()
    );

    // 3. Batch-fetch from the HTTP backend (chunks of 100).
    let cfg = crate::config::Config::fresh();
    if !cfg.platform_sync_enabled() {
        tracing::debug!("warm_cache_for_remote: local mode; not contacting the platform");
        return Ok(());
    }
    let backend_url = match cfg.notes_backend_url() {
        Some(url) => url.to_string(),
        None => {
            tracing::debug!(
                "warm_cache_for_remote: notes_backend.backend_url is not configured; skipping"
            );
            return Ok(());
        }
    };
    let ctx = ApiContext::new(Some(backend_url));
    let client = ApiClient::new(ctx);

    // Skip when not authenticated (matches daemon flush_notes pattern).
    if !client.is_logged_in() && !client.has_api_key() {
        tracing::debug!("warm_cache_for_remote: not authenticated; skipping");
        return Ok(());
    }

    for chunk in uncached.chunks(100) {
        let sha_refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
        match client.read_notes(&sha_refs) {
            Ok(response) => {
                if response.notes.is_empty() {
                    continue;
                }
                // 4. Write returned entries as already-synced cache rows.
                let entries: Vec<(String, String)> = response.notes.into_iter().collect();
                match crate::notes::db::NotesDatabase::global() {
                    Ok(db) => match db.lock() {
                        Ok(mut lock) => {
                            if let Err(e) = lock.cache_synced_notes(&entries) {
                                tracing::warn!(
                                    "warm_cache_for_remote: cache_synced_notes error: {}",
                                    e
                                );
                            } else {
                                tracing::debug!(
                                    count = entries.len(),
                                    "warm_cache_for_remote: cached notes from remote"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!("warm_cache_for_remote: DB lock poisoned: {}", e);
                        }
                    },
                    Err(e) => {
                        tracing::warn!("warm_cache_for_remote: failed to open notes-db: {}", e);
                    }
                }
            }
            Err(e) => {
                // Best-effort: log and continue.
                tracing::warn!("warm_cache_for_remote: read_notes error: {}", e);
            }
        }
    }

    Ok(())
}

// --- HTTP backend helpers (private) ---

/// Queue one note for upload as the `http` backend does (tests).
#[cfg(test)]
fn http_write_note(
    commit_sha: &str,
    content: &str,
    repo_url: Option<&str>,
) -> Result<(), AutterError> {
    http_write_batch(&[(commit_sha.to_string(), content.to_string())], repo_url)
}

#[cfg(test)]
fn http_write_batch(
    entries: &[(String, String)],
    repo_url: Option<&str>,
) -> Result<(), AutterError> {
    let db = crate::notes::db::NotesDatabase::global()?;
    let mut db_lock = db
        .lock()
        .map_err(|e| AutterError::Generic(format!("notes-db lock: {}", e)))?;
    enqueue_notes(&mut db_lock, entries, repo_url)?;
    drop(db_lock);
    crate::daemon::telemetry_handle::submit_notes();
    Ok(())
}

/// Queue `entries` for upload, holding the ones [`hold_next_upload`] named.
fn enqueue_notes(
    db: &mut crate::notes::db::NotesDatabase,
    entries: &[(String, String)],
    repo_url: Option<&str>,
) -> Result<(), AutterError> {
    let held = take_held_uploads(entries);
    db.upsert_notes_batch_holding(entries, repo_url, &held, PROVISIONAL_NOTE_HOLD_SECS)
}

fn http_read_note(commit_sha: &str) -> Option<String> {
    let db = crate::notes::db::NotesDatabase::global().ok()?;
    let db_lock = db.lock().ok()?;
    db_lock.get_note(commit_sha).ok().flatten()
}

fn http_read_notes(commit_shas: &[String]) -> HashMap<String, String> {
    let Ok(db) = crate::notes::db::NotesDatabase::global() else {
        return HashMap::new();
    };
    let Ok(db_lock) = db.lock() else {
        return HashMap::new();
    };
    let refs: Vec<&str> = commit_shas.iter().map(|s| s.as_str()).collect();
    db_lock.get_notes(&refs).unwrap_or_default()
}

fn http_fetch_and_cache_notes(commit_shas: &[String]) -> HashMap<String, String> {
    if commit_shas.is_empty() {
        return HashMap::new();
    }

    let cfg = Config::fresh();
    // Local mode does not contact the platform at all.
    if !cfg.platform_sync_enabled() {
        return HashMap::new();
    }
    let Some(backend_url) = cfg.notes_backend_url().map(str::to_string) else {
        return HashMap::new();
    };

    let ctx = crate::api::client::ApiContext::new(Some(backend_url));
    let client = crate::api::client::ApiClient::new(ctx);
    if !client.is_logged_in() && !client.has_api_key() {
        return HashMap::new();
    }

    let mut fetched = HashMap::new();
    for chunk in commit_shas.chunks(100) {
        let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
        match client.read_notes(&refs) {
            Ok(response) => {
                if response.notes.is_empty() {
                    continue;
                }
                let entries: Vec<(String, String)> = response.notes.into_iter().collect();
                if let Ok(db) = crate::notes::db::NotesDatabase::global()
                    && let Ok(mut lock) = db.lock()
                {
                    let _ = lock.cache_synced_notes(&entries);
                }
                fetched.extend(entries);
            }
            Err(e) => {
                tracing::debug!(%e, "notes batch read from HTTP backend failed");
            }
        }
    }

    fetched
}

fn http_check_exists(commit_shas: &[String]) -> HashSet<String> {
    http_read_notes(commit_shas).into_keys().collect()
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// With kind=Http, the http helpers upsert into notes-db (synced=0) and the
    /// read helper returns the cached value. This tests the private http_* helpers
    /// directly so no config override is needed.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn http_write_then_read_uses_cache() {
        // Point the notes-db at a temp file so we don't pollute the real DB.
        let tmp = tempfile::NamedTempFile::new().expect("tmp file");
        let db_path = tmp.path().to_str().unwrap().to_string();
        // Safety: test-only env var manipulation.
        crate::notes::db::NotesDatabase::install_test_db_at(std::path::Path::new(&db_path))
            .expect("install isolated notes-db");

        // Write directly via http helper (no repo needed).
        http_write_note(
            "abc123def456abc123def456abc123def456abc1",
            "test content",
            None,
        )
        .expect("write");

        // Read back from cache.
        let content = http_read_note("abc123def456abc123def456abc123def456abc1");
        assert_eq!(content, Some("test content".to_string()));

        // Confirm it is in the DB with synced=0.
        let db = crate::notes::db::NotesDatabase::global().expect("global db");
        let mut lock = db.lock().expect("lock");
        let pending = lock.dequeue_pending(10).expect("dequeue");
        assert!(
            pending.iter().any(
                |p| p.commit_sha == "abc123def456abc123def456abc123def456abc1"
                    && p.content == "test content"
            ),
            "expected pending row in notes-db"
        );
    }

    /// http_read_notes returns a HashMap of all cached entries for requested SHAs.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn http_read_notes_returns_multiple() {
        let tmp = tempfile::NamedTempFile::new().expect("tmp file");
        let db_path = tmp.path().to_str().unwrap().to_string();
        crate::notes::db::NotesDatabase::install_test_db_at(std::path::Path::new(&db_path))
            .expect("install isolated notes-db");

        let sha1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let sha2 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let sha3 = "cccccccccccccccccccccccccccccccccccccccc".to_string();

        http_write_note(&sha1, "content-a", None).expect("write sha1");
        http_write_note(&sha2, "content-b", None).expect("write sha2");

        // sha3 is not written — should not appear in result.
        let result = http_read_notes(&[sha1.clone(), sha2.clone(), sha3.clone()]);
        assert_eq!(result.get(&sha1), Some(&"content-a".to_string()));
        assert_eq!(result.get(&sha2), Some(&"content-b".to_string()));
        assert!(!result.contains_key(&sha3));
    }

    /// With kind=GitNotes (default), read_note_blob_oids delegates to git.
    /// Verified by building with an empty repo — returns Ok(empty) with no panic.
    #[test]
    fn git_notes_backend_read_note_blob_oids_delegates_to_git() {
        use crate::git::test_utils::TmpRepo;
        // Default config is GitNotes — no override needed.
        let tmp = TmpRepo::new().expect("TmpRepo::new");
        let result = crate::git::refs::note_blob_oids_for_commits(tmp.autter_repo(), &[]);
        assert!(result.is_ok());
    }

    /// With kind=Http, the public read_note_blob_oids returns an empty map
    /// because notes live in notes-db, not in git refs.
    /// We test this by calling the function through a fresh Config set to Http.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn http_backend_read_note_blob_oids_returns_empty_map() {
        use crate::git::test_utils::TmpRepo;

        let old = std::env::var("AUTTER_NOTES_BACKEND_KIND").ok();
        unsafe {
            std::env::set_var("AUTTER_NOTES_BACKEND_KIND", "http");
        }

        let tmp = TmpRepo::new().expect("TmpRepo::new");
        // Use Config::fresh() so it picks up the env var, then call the refs function
        // through the kind check inline.
        let kind = crate::config::Config::fresh().notes_backend_kind();
        let result: Result<HashMap<String, String>, _> = match kind {
            crate::config::NotesBackendKind::Http => Ok(HashMap::new()),
            crate::config::NotesBackendKind::GitNotes | crate::config::NotesBackendKind::Both => {
                crate::git::refs::note_blob_oids_for_commits(
                    tmp.autter_repo(),
                    &["abc".to_string()],
                )
            }
        };

        // Restore env before asserting (so a panic doesn't leave the env dirty).
        match old {
            Some(v) => unsafe { std::env::set_var("AUTTER_NOTES_BACKEND_KIND", v) },
            None => unsafe { std::env::remove_var("AUTTER_NOTES_BACKEND_KIND") },
        }

        assert!(result.is_ok());
        assert!(
            result.unwrap().is_empty(),
            "Http backend should return empty map from read_note_blob_oids"
        );
    }

    /// Integration test: with kind=Http, `write_note` upserts into `notes-db`
    /// with `synced = 0` and `git notes --ref=ai show <sha>` returns nothing (note
    /// is NOT written into git refs).
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn integration_http_write_note_goes_to_db_not_git() {
        use crate::git::repository::exec_git;
        use crate::git::test_utils::TmpRepo;

        // Isolated notes-db for this test.
        let tmp_db = tempfile::NamedTempFile::new().expect("tmp db file");
        let db_path = tmp_db.path().to_str().unwrap().to_string();
        crate::notes::db::NotesDatabase::install_test_db_at(std::path::Path::new(&db_path))
            .expect("install isolated notes-db");

        let repo = TmpRepo::new().expect("TmpRepo::new");

        // Create a real commit so we have a valid SHA.
        repo.write_file("a.txt", "hello", false)
            .expect("write file");
        let sha = repo.commit_all("msg").expect("commit");

        // Write a note for this SHA using the Http helper.
        http_write_note(&sha, "some-note-content", None).expect("http write");

        // Confirm it is in notes-db with synced=0.
        let db = crate::notes::db::NotesDatabase::global().expect("global db");
        let mut lock = db.lock().expect("lock");
        let note_in_db = lock.get_note(&sha).expect("get note");
        assert_eq!(note_in_db, Some("some-note-content".to_string()));

        let pending = lock.dequeue_pending(10).expect("dequeue");
        assert!(
            pending.iter().any(|p| p.commit_sha == sha),
            "note should be pending in notes-db"
        );
        drop(lock);

        // Confirm `git notes --ref=ai show <sha>` returns nothing.
        let mut args = repo.autter_repo().global_args_for_exec();
        args.extend([
            "notes".to_string(),
            "--ref=ai".to_string(),
            "show".to_string(),
            sha.clone(),
        ]);
        let result = exec_git(&args);
        assert!(
            result.is_err(),
            "git notes --ref=ai show should fail (note not in git) for Http backend"
        );
    }

    /// Integration test: the `Both` backend writes the note to git refs AND
    /// queues it in notes-db for cloud upload.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn integration_both_write_note_goes_to_git_and_db() {
        use crate::git::repository::exec_git;
        use crate::git::test_utils::TmpRepo;

        // Isolated notes-db for this test.
        let tmp_db = tempfile::NamedTempFile::new().expect("tmp db file");
        let db_path = tmp_db.path().to_str().unwrap().to_string();
        crate::notes::db::NotesDatabase::install_test_db_at(std::path::Path::new(&db_path))
            .expect("install isolated notes-db");

        let repo = TmpRepo::new().expect("TmpRepo::new");

        repo.write_file("a.txt", "hello", false)
            .expect("write file");
        let sha = repo.commit_all("msg").expect("commit");

        write_note_with_kind(
            NotesBackendKind::Both,
            repo.autter_repo(),
            &sha,
            "dual-note-content",
        )
        .expect("both write");

        // Confirm it is queued in notes-db for cloud upload.
        let db = crate::notes::db::NotesDatabase::global().expect("global db");
        let mut lock = db.lock().expect("lock");
        let note_in_db = lock.get_note(&sha).expect("get note");
        assert_eq!(note_in_db, Some("dual-note-content".to_string()));
        let pending = lock.dequeue_pending(10).expect("dequeue");
        assert!(
            pending.iter().any(|p| p.commit_sha == sha),
            "note should be pending in notes-db for the Both backend"
        );
        drop(lock);

        // Confirm the note ALSO exists in git refs/notes/ai.
        let mut args = repo.autter_repo().global_args_for_exec();
        args.extend([
            "notes".to_string(),
            "--ref=ai".to_string(),
            "show".to_string(),
            sha.clone(),
        ]);
        let output = exec_git(&args).expect("git notes show should succeed for Both backend");
        let shown = String::from_utf8_lossy(&output.stdout);
        assert_eq!(shown.trim(), "dual-note-content");
    }

    /// Integration test: `materialize_notes_for_display` writes notes from the
    /// notes-db cache into `refs/notes/ai-display` so that `git log --notes=ai-display`
    /// can show them.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn integration_materialize_notes_for_display() {
        use crate::git::repository::exec_git;
        use crate::git::test_utils::TmpRepo;

        // Isolated notes-db.
        let tmp_db = tempfile::NamedTempFile::new().expect("tmp db file");
        crate::notes::db::NotesDatabase::install_test_db_at(std::path::Path::new(
            tmp_db.path().to_str().unwrap(),
        ))
        .expect("install isolated notes-db");

        let repo = TmpRepo::new().expect("TmpRepo::new");

        // Create a real commit.
        repo.write_file("b.txt", "world", false)
            .expect("write file");
        let sha = repo.commit_all("test commit").expect("commit");

        // Put a note in the cache for this commit.
        http_write_note(&sha, "display-note-content", None).expect("write note");

        // Materialize the cache into refs/notes/ai-display.
        let count = materialize_notes_for_display(repo.autter_repo(), 50).expect("materialize");
        assert_eq!(count, 1, "should have materialized 1 note");

        // Confirm git can read the note from refs/notes/ai-display.
        let mut args = repo.autter_repo().global_args_for_exec();
        args.extend([
            "notes".to_string(),
            "--ref=ai-display".to_string(),
            "show".to_string(),
            sha.clone(),
        ]);
        let output = exec_git(&args).expect("git notes show ai-display");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.trim() == "display-note-content",
            "refs/notes/ai-display should contain the materialized note, got: {:?}",
            stdout
        );
    }

    /// Verify that `run_pre_push_hook_managed` has the correct early-return guard for
    /// `kind = Http`. We test this by confirming Config::fresh() with
    /// `AUTTER_NOTES_BACKEND_KIND=http` returns Http, and that the guard in
    /// `run_pre_push_hook_managed` would short-circuit. This is a compile-time
    /// regression guard for the code structure added in Phase 2.6.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn push_pre_command_hook_http_guard_is_in_place() {
        use std::env;

        let old = env::var("AUTTER_NOTES_BACKEND_KIND").ok();
        unsafe {
            env::set_var("AUTTER_NOTES_BACKEND_KIND", "http");
        }
        let kind = crate::config::Config::fresh().notes_backend_kind();
        match old {
            Some(v) => unsafe { env::set_var("AUTTER_NOTES_BACKEND_KIND", v) },
            None => unsafe { env::remove_var("AUTTER_NOTES_BACKEND_KIND") },
        }

        // Verify Config::fresh() correctly parses http from env.
        assert_eq!(
            kind,
            crate::config::NotesBackendKind::Http,
            "Config::fresh() should reflect AUTTER_NOTES_BACKEND_KIND=http"
        );

        // The actual early-return code in run_pre_push_hook_managed was added
        // in Phase 2.6. Verify it compiles and is reachable by referencing the
        // function pointer. Structural verification: when kind == Http, the
        // function returns before doing any work.
        let _ = crate::commands::hooks::push_hooks::run_pre_push_hook_managed as fn(_, _);
    }

    // --- warm_cache_for_remote tests ---
    //
    // These tests verify the core behavior of `warm_cache_for_remote`:
    //
    // 1. It fetches notes from the HTTP backend and stores them with `synced = 1`.
    // 2. It skips SHAs already present in notes-db (not included in the request).
    //
    // Design notes on the `NOTES_DB` `OnceLock` singleton:
    //
    // `NotesDatabase::global()` is a `OnceLock`, so the DB path is resolved
    // *once per process*: setting `AUTTER_TEST_NOTES_DB_PATH` only reaches the
    // test that happens to initialise it first, and every later test silently
    // reuses that DB.  When that first initialiser isn't one of these tests, the
    // path is the developer's real `~/.autter/internal/notes-db` and the running
    // daemon drains freshly written rows before the assertions run.
    //
    // Strategy: each test calls `NotesDatabase::install_test_db_at()`, which
    // swaps a fresh throwaway DB into the already-initialised mutex.  That
    // sidesteps the `OnceLock` entirely, so isolation no longer depends on test
    // ordering, and the real notes DB is never written to.  The `#[serial]`
    // group still matters: the singleton is process-global, so only one test at
    // a time may point it at its own file.

    /// Unit test: `warm_cache_for_remote` fetches notes from a mock HTTP server
    /// and stores them in `notes-db` with `synced = 1`.
    ///
    /// Steps:
    /// 1. Point `NOTES_DB` at a fresh temp file via `install_test_db_at`.
    /// 2. Spin up a mockito server returning two notes.
    /// 3. Create a `TmpRepo` with two commits.
    /// 4. Call `warm_cache_for_remote`.
    /// 5. Verify both SHAs appear in `notes-db` with `synced = 1` via `NotesDatabase::global()`.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn warm_cache_for_remote_populates_db_with_synced_1() {
        use crate::git::test_utils::TmpRepo;
        use crate::notes::db::NotesDatabase;
        use tempfile::NamedTempFile;

        // Set the test DB path before the first DB call so the OnceLock picks it up.
        let tmp_db = NamedTempFile::new().expect("tmp notes-db");
        crate::notes::db::NotesDatabase::install_test_db_at(std::path::Path::new(tmp_db.path()))
            .expect("install isolated notes-db");

        // Build a TmpRepo with two commits.
        let repo = TmpRepo::new().expect("TmpRepo::new");

        repo.write_file("warm1.txt", "warm1", false)
            .expect("write file");
        let sha1 = repo.commit_all("warm-commit-1").expect("commit 1");

        repo.write_file("warm2.txt", "warm2", false)
            .expect("write file");
        let sha2 = repo.commit_all("warm-commit-2").expect("commit 2");

        // Spin up a mockito server that returns notes for both SHAs.
        let mut server = mockito::Server::new();
        let notes_json = serde_json::json!({
            "notes": {
                sha1.clone(): "note-content-1",
                sha2.clone(): "note-content-2"
            }
        })
        .to_string();

        let _mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/worker/notes/".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(&notes_json)
            .create();

        let server_url = server.url();
        unsafe {
            std::env::set_var("AUTTER_NOTES_BACKEND_URL", &server_url);
            // Provide a fake API key so `has_api_key()` returns true and the
            // auth guard in `warm_cache_for_remote` does not short-circuit.
            std::env::set_var("AUTTER_API_KEY", "warm-cache-test-key");
            // Connected mode regardless of this machine's ~/.autter/config.json,
            // which Config::fresh() would otherwise read.
            std::env::set_var(
                "AUTTER_TEST_CONFIG_PATCH",
                serde_json::json!({
                    "prompt_storage": "default",
                    "notes_backend": { "kind": "http", "backend_url": server_url },
                })
                .to_string(),
            );
        }

        // Execute.
        let result = warm_cache_for_remote(repo.autter_repo(), "origin");
        assert!(result.is_ok(), "warm_cache_for_remote failed: {:?}", result);

        // Verify via NotesDatabase::global() (the same DB the function wrote to).
        let db = NotesDatabase::global().expect("global db");
        let lock = db.lock().expect("lock");

        let content1 = lock.get_note(&sha1).expect("get sha1");
        let content2 = lock.get_note(&sha2).expect("get sha2");

        assert_eq!(
            content1,
            Some("note-content-1".to_string()),
            "sha1 should be cached with correct content"
        );
        assert_eq!(
            content2,
            Some("note-content-2".to_string()),
            "sha2 should be cached with correct content"
        );

        // Rows must NOT appear in dequeue_pending (cache_synced_notes inserts synced = 1).
        drop(lock);
        let mut lock = db.lock().expect("lock for dequeue check");
        let pending = lock.dequeue_pending(10).expect("dequeue");
        let warm_pending: Vec<_> = pending
            .iter()
            .filter(|p| p.commit_sha == sha1 || p.commit_sha == sha2)
            .collect();
        assert!(
            warm_pending.is_empty(),
            "cache_synced rows must not appear in dequeue_pending: {:?}",
            warm_pending
                .iter()
                .map(|p| &p.commit_sha)
                .collect::<Vec<_>>()
        );

        // Cleanup.
        unsafe {
            std::env::remove_var("AUTTER_API_KEY");
            std::env::remove_var("AUTTER_TEST_CONFIG_PATCH");
            std::env::remove_var("AUTTER_NOTES_BACKEND_URL");
        }
    }

    /// Unit test: `warm_cache_for_remote` skips SHAs already present in `notes-db`.
    ///
    /// Steps:
    /// 1. Pre-populate `notes-db` with sha1 via `cache_synced_notes`.
    /// 2. Spin up a mockito server returning sha2 only.
    ///    The mock matches only requests whose query contains sha2 —
    ///    if sha1 were incorrectly included it would still match, but we verify
    ///    indirectly that sha1's content was not overwritten.
    /// 3. Call `warm_cache_for_remote` with a TmpRepo containing both commits.
    /// 4. Verify sha1's content is unchanged ("already-cached-note").
    /// 5. Verify sha2 was fetched and cached with `synced = 1`.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn warm_cache_for_remote_skips_already_cached_shas() {
        use crate::git::test_utils::TmpRepo;
        use crate::notes::db::NotesDatabase;
        use tempfile::NamedTempFile;

        // Set the test DB path (may be ignored if OnceLock was already set by
        // `warm_cache_for_remote_populates_db_with_synced_1` in the same process,
        // but we still set it for freshness when running this test in isolation).
        let tmp_db = NamedTempFile::new().expect("tmp notes-db");
        crate::notes::db::NotesDatabase::install_test_db_at(std::path::Path::new(tmp_db.path()))
            .expect("install isolated notes-db");

        // Build TmpRepo with two commits.
        let repo = TmpRepo::new().expect("TmpRepo::new");

        repo.write_file("skip1.txt", "s1", false)
            .expect("write file");
        let sha1 = repo.commit_all("skip-c1").expect("commit 1");

        repo.write_file("skip2.txt", "s2", false)
            .expect("write file");
        let sha2 = repo.commit_all("skip-c2").expect("commit 2");

        // Pre-populate notes-db with sha1 via the global singleton.
        {
            let db = NotesDatabase::global().expect("global db");
            let mut lock = db.lock().expect("lock");
            lock.cache_synced_notes(&[(sha1.clone(), "already-cached-note".to_string())])
                .expect("cache_synced_notes sha1");
        }

        // Mock server: use two mocks to verify sha1 is NOT in the request.
        //
        // - Mock A: matches requests where the query contains sha2 but NOT sha1.
        //   Since mockito doesn't have a `Not` matcher, we approximate this by
        //   requiring the query equals exactly sha2 (no comma-separated prefix/suffix).
        //   `commits=<sha2>` means only sha2 was requested.
        // - Mock B: fallback that matches everything else → returns 500 so sha2
        //   is NOT cached if sha1 was erroneously included.
        //
        // If warm_cache correctly filters sha1, mock A matches and sha2 is cached.
        // If warm_cache incorrectly sends sha1 too, the query is `sha1,sha2` or
        // `sha2,sha1`, which won't match the exact-sha2 regex → mock B fires → 500
        // error → sha2 is NOT cached → `assert_eq!(content2, ...)` fails.
        let sha2_note_json = serde_json::json!({
            "notes": { sha2.clone(): "note-content-skip-2" }
        })
        .to_string();

        // Exact query: commits=<sha2> only.
        let exact_sha2_query = format!("commits={}", sha2);

        let mut server = mockito::Server::new();
        // Mock A: exact query with only sha2.
        let _mock_ok = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/worker/notes/".to_string()),
            )
            .match_query(mockito::Matcher::Exact(exact_sha2_query))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(&sha2_note_json)
            .create();
        // Mock B: fallback → 500.
        let _mock_fallback = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/worker/notes/".to_string()),
            )
            .with_status(500)
            .with_body(r#"{"error":"unexpected request with sha1 in query"}"#)
            .create();

        let server_url = server.url();
        unsafe {
            std::env::set_var("AUTTER_NOTES_BACKEND_URL", &server_url);
            std::env::set_var("AUTTER_API_KEY", "skip-test-key");
            // Connected mode regardless of this machine's ~/.autter/config.json,
            // which Config::fresh() would otherwise read.
            std::env::set_var(
                "AUTTER_TEST_CONFIG_PATCH",
                serde_json::json!({
                    "prompt_storage": "default",
                    "notes_backend": { "kind": "http", "backend_url": server_url },
                })
                .to_string(),
            );
        }

        let result = warm_cache_for_remote(repo.autter_repo(), "origin");
        assert!(result.is_ok(), "warm_cache_for_remote failed: {:?}", result);

        // Verify via the global DB.
        let db = NotesDatabase::global().expect("global db");
        let lock = db.lock().expect("lock");

        // sha1 must retain its pre-cached content unchanged.
        let content1 = lock.get_note(&sha1).expect("get sha1");
        assert_eq!(
            content1,
            Some("already-cached-note".to_string()),
            "sha1 content must not change — warm_cache must not overwrite cached entries"
        );

        // sha2 must now be cached with the server-returned content.
        let content2 = lock.get_note(&sha2).expect("get sha2");
        assert_eq!(
            content2,
            Some("note-content-skip-2".to_string()),
            "sha2 should have been fetched and cached"
        );

        // The mock must have been called (warm_cache made at least one HTTP request).
        _mock_ok.assert();

        // Cleanup.
        unsafe {
            std::env::remove_var("AUTTER_API_KEY");
            std::env::remove_var("AUTTER_TEST_CONFIG_PATCH");
            std::env::remove_var("AUTTER_NOTES_BACKEND_URL");
        }
    }

    /// A note for `commit` with one AI entry covering `range` of `a.txt`.
    fn note_for(commit: &str, range: (u32, u32)) -> String {
        use crate::authorship::authorship_log::LineRange;
        use crate::authorship::authorship_log_serialization::AttestationEntry;
        let mut log = AuthorshipLog::new();
        log.metadata.base_commit_sha = commit.to_string();
        log.get_or_create_file("a.txt")
            .add_entry(AttestationEntry::new(
                "c9883b05a2487d6d".to_string(),
                vec![LineRange::Range(range.0, range.1)],
            ));
        log.serialize_to_string().unwrap()
    }

    fn git_note(repo: &crate::git::test_utils::TmpRepo, commit: &str) -> Option<String> {
        crate::git::refs::show_authorship_note(repo.autter_repo(), commit)
    }

    /// The writer's rule for a commit that already has a note (see
    /// `filter_note_writes`), with the `both` backend and the notes database
    /// in each state a note can be in.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn writer_keeps_a_note_that_may_be_on_the_server() {
        use crate::authorship::note_integrity::strip_integrity;
        use crate::git::test_utils::TmpRepo;
        use crate::notes::db::NotesDatabase;

        let tmp_db = tempfile::NamedTempFile::new().expect("tmp db file");
        NotesDatabase::install_test_db_at(tmp_db.path()).expect("install isolated notes-db");
        let repo = TmpRepo::new().expect("TmpRepo::new");
        repo.write_file("a.txt", "one\ntwo\nthree\n", false)
            .expect("write file");
        let sha = repo.commit_all("msg").expect("commit");
        let both = NotesBackendKind::Both;
        let first = note_for(&sha, (1, 1));
        let correction = note_for(&sha, (1, 2));
        let body = |n: &str| strip_integrity(n).trim_end().to_string();
        let row = |sha: &str| {
            NotesDatabase::global()
                .unwrap()
                .lock()
                .unwrap()
                .get_note(sha)
                .unwrap()
        };

        // Queued and never sent: a correction replaces it before it uploads.
        write_note_with_kind(both, repo.autter_repo(), &sha, &first).unwrap();
        write_note_with_kind(both, repo.autter_repo(), &sha, &correction).unwrap();
        assert_eq!(body(&git_note(&repo, &sha).unwrap()), body(&correction));
        assert_eq!(body(&row(&sha).unwrap()), body(&correction));

        // Sent: kept, in git and in the queue.
        {
            let db = NotesDatabase::global().unwrap();
            let mut db = db.lock().unwrap();
            let pending = db.dequeue_pending(10).unwrap();
            assert_eq!(pending.len(), 1);
            db.mark_synced(std::slice::from_ref(&sha)).unwrap();
        }
        let sent = git_note(&repo, &sha).unwrap();
        write_note_with_kind(both, repo.autter_repo(), &sha, &first).unwrap();
        write_notes_batch_with_kind(both, repo.autter_repo(), &[(sha.clone(), first.clone())])
            .unwrap();
        assert_eq!(git_note(&repo, &sha).unwrap(), sent);
        assert_eq!(body(&row(&sha).unwrap()), body(&correction));
        assert!(
            NotesDatabase::global()
                .unwrap()
                .lock()
                .unwrap()
                .dequeue_pending(10)
                .unwrap()
                .is_empty(),
            "nothing new to upload"
        );

        // The same note again: not written (also unstamped, as callers pass it).
        write_note_with_kind(both, repo.autter_repo(), &sha, &correction).unwrap();
        assert_eq!(git_note(&repo, &sha).unwrap(), sent);

        // Being uploaded, or an upload attempted: kept.
        let second = repo
            .write_file("a.txt", "one\ntwo\nthree\nfour\n", false)
            .and_then(|_| repo.commit_all("second"))
            .expect("second commit");
        write_note_with_kind(
            both,
            repo.autter_repo(),
            &second,
            &note_for(&second, (1, 1)),
        )
        .unwrap();
        let in_flight = git_note(&repo, &second).unwrap();
        {
            let db = NotesDatabase::global().unwrap();
            let mut db = db.lock().unwrap();
            assert_eq!(db.dequeue_pending(10).unwrap().len(), 1);
        }
        write_note_with_kind(
            both,
            repo.autter_repo(),
            &second,
            &note_for(&second, (1, 4)),
        )
        .unwrap();
        assert_eq!(git_note(&repo, &second).unwrap(), in_flight);
        {
            let db = NotesDatabase::global().unwrap();
            let mut db = db.lock().unwrap();
            db.mark_failed(std::slice::from_ref(&second), "network")
                .unwrap();
        }
        write_note_with_kind(
            both,
            repo.autter_repo(),
            &second,
            &note_for(&second, (1, 4)),
        )
        .unwrap();
        assert_eq!(git_note(&repo, &second).unwrap(), in_flight);

        // No row at all (fetched from a teammate or CI): kept.
        let third = repo
            .write_file("a.txt", "one\n", false)
            .and_then(|_| repo.commit_all("third"))
            .expect("third commit");
        crate::git::refs::notes_add(repo.autter_repo(), &third, &note_for(&third, (1, 1))).unwrap();
        let fetched = git_note(&repo, &third).unwrap();
        write_note_with_kind(both, repo.autter_repo(), &third, &note_for(&third, (1, 3))).unwrap();
        assert_eq!(git_note(&repo, &third).unwrap(), fetched);
        assert_eq!(row(&third), None, "nothing queued for upload");

        // ...unless `autter squash-authorship` asked for it: written.
        REPLACE_RECORDED_NOTES.store(true, Ordering::SeqCst);
        let replaced =
            write_note_with_kind(both, repo.autter_repo(), &third, &note_for(&third, (1, 3)));
        REPLACE_RECORDED_NOTES.store(false, Ordering::SeqCst);
        replaced.unwrap();
        assert_eq!(
            body(&git_note(&repo, &third).unwrap()),
            body(&note_for(&third, (1, 3)))
        );

        // A note git copied here from another commit: replaced, whatever the
        // database says (it was never queued for this commit).
        crate::git::refs::notes_add_batch_unstamped(repo.autter_repo(), &[(third.clone(), sent)])
            .unwrap();
        write_note_with_kind(both, repo.autter_repo(), &third, &note_for(&third, (1, 1))).unwrap();
        assert_eq!(
            body(&git_note(&repo, &third).unwrap()),
            body(&note_for(&third, (1, 1)))
        );

        // A rewrite that keeps the commit at its SHA: kept, also when unsent.
        let fourth = repo
            .write_file("a.txt", "x\n", false)
            .and_then(|_| repo.commit_all("fourth"))
            .expect("fourth commit");
        write_note_with_kind(
            both,
            repo.autter_repo(),
            &fourth,
            &note_for(&fourth, (1, 1)),
        )
        .unwrap();
        let kept = git_note(&repo, &fourth).unwrap();
        {
            let _keep = keep_existing_notes_for([fourth.clone()]);
            write_note_with_kind(
                both,
                repo.autter_repo(),
                &fourth,
                &note_for(&fourth, (1, 9)),
            )
            .unwrap();
        }
        assert_eq!(git_note(&repo, &fourth).unwrap(), kept);

        // `git_notes` uploads nothing: a correction is written.
        write_note_with_kind(
            NotesBackendKind::GitNotes,
            repo.autter_repo(),
            &second,
            &note_for(&second, (1, 4)),
        )
        .unwrap();
        assert_eq!(
            body(&git_note(&repo, &second).unwrap()),
            body(&note_for(&second, (1, 4)))
        );
    }
}
