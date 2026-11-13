//! Where stamping began: the earliest commits whose authorship note carries
//! an integrity stamp (`note_integrity`).
//!
//! A note without a stamp reads as legacy, as autter releases before 3.8.0
//! wrote it. On a commit made after commits with stamped notes, that claim
//! needs a second look (`note_integrity::verify_notes`): either the note was
//! written by an older autter (a teammate who has not upgraded) or its stamp
//! was removed.
//!
//! **What "after" means.** Commit topology, not dates: a commit comes after
//! stamping began when it descends from a commit whose note is stamped.
//! Commit dates are set by whoever makes the commit and change on a rebase,
//! so a date cut-off would let a forged or rebased date move a commit to
//! either side of it. Dates are used here only to choose the order in which
//! the floor is built, which changes how many git calls that takes, never
//! the result.
//!
//! **The floor.** The stamped commits that have no stamped ancestor (the
//! minimal elements of the set of stamped commits; on a linear history, the
//! first one). A commit descends from some stamped commit exactly when it
//! descends from a floor commit. Because no floor commit is an ancestor of
//! another, `git rev-list --ancestry-path <commits> --not <floor>` lists
//! every one of `commits` that descends from the floor, in one call.
//!
//! **Cost.** Only notes without a stamp ask the question, so a repository
//! whose checked notes are all stamped never computes the floor. When it is
//! needed, it is computed once per process (and notes ref), and kept in
//! `.git/ai/stamp_floor.json` keyed by the tree of `refs/notes/ai`. When the
//! notes ref has moved since, only the notes that changed are read (one
//! `git diff-tree` and one `git cat-file`), and the floor is extended with
//! the new stamped ones. It is computed again from every note (one
//! `git grep` over the notes tree) only when there is no usable cache or a
//! floor commit's note changed or was removed.
//!
//! **Limits.** Notes are read from `refs/notes/ai`. With the `http` notes
//! backend there is no local notes ref, so the floor is empty and only the
//! version rule in `note_integrity` applies (the server's record covers
//! those notes). Commits whose notes are stamped but which are not in this
//! clone are left out of the floor.

use crate::error::AutterError;
use crate::git::refs::AI_AUTHORSHIP_FULL_REF;
use crate::git::repository::{Repository, exec_git, exec_git_allow_nonzero, exec_git_stdin};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// What a stamped note looks like in the notes tree (`git grep -F`).
const STAMP_PATTERN: &str = "\"integrity\": {";

/// The cache file's format version.
const CACHE_VERSION: u32 = 1;

/// Upper bound on the commits the floor may hold; past it the floor stops
/// growing (the check is then less strict, never stricter).
const MAX_FLOOR: usize = 256;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    v: u32,
    notes_tree: String,
    floor: Vec<String>,
}

/// The floor of the repository's notes, as of the current `refs/notes/ai`.
/// Empty when there are no stamped notes, no notes ref, or the floor could
/// not be computed (the check is then skipped, never failed).
pub fn stamp_floor(repo: &Repository) -> Vec<String> {
    match try_stamp_floor(repo) {
        Ok(floor) => floor,
        Err(e) => {
            tracing::debug!("note integrity: stamp floor not computed: {e}");
            Vec::new()
        }
    }
}

/// The commits among `commits` made after stamping began: each descends from
/// a commit whose note is stamped. Never fails: on any error, none.
pub fn after_stamping(repo: &Repository, commits: &[String]) -> HashSet<String> {
    if commits.is_empty() {
        return HashSet::new();
    }
    let floor = stamp_floor(repo);
    if floor.is_empty() {
        return HashSet::new();
    }
    match descendants_among(repo, &floor, commits) {
        Ok(after) => after,
        Err(e) => {
            tracing::debug!("note integrity: ancestry of unstamped notes not checked: {e}");
            HashSet::new()
        }
    }
}

/// Whether `commit` is a commit whose note is in the floor, or descends
/// from one.
pub fn at_or_after_stamping(repo: &Repository, commit: &str) -> bool {
    let floor = stamp_floor(repo);
    if floor.iter().any(|c| c == commit) {
        return true;
    }
    match descendants_among(repo, &floor, &[commit.to_string()]) {
        Ok(after) => after.contains(commit),
        Err(e) => {
            tracing::debug!("note integrity: ancestry of {commit} not checked: {e}");
            false
        }
    }
}

fn memo() -> &'static Mutex<HashMap<(PathBuf, String), Vec<String>>> {
    static MEMO: OnceLock<Mutex<HashMap<(PathBuf, String), Vec<String>>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn try_stamp_floor(repo: &Repository) -> Result<Vec<String>, AutterError> {
    let Some(notes_tree) = notes_tree(repo)? else {
        return Ok(Vec::new());
    };
    let cache_path = repo.storage.ai_dir.join("stamp_floor.json");
    let key = (cache_path.clone(), notes_tree.clone());
    if let Some(floor) = memo().lock().ok().and_then(|m| m.get(&key).cloned()) {
        return Ok(floor);
    }

    let cached = std::fs::read(&cache_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<CacheFile>(&bytes).ok())
        .filter(|c| c.v == CACHE_VERSION);
    let floor = match cached {
        Some(c) if c.notes_tree == notes_tree => c.floor,
        Some(c) => match extend_from_diff(repo, &c.notes_tree, &notes_tree, c.floor) {
            Ok(Some(floor)) => floor,
            Ok(None) => full_floor(repo, &notes_tree)?,
            Err(e) => {
                tracing::debug!("note integrity: stamp floor rebuilt: {e}");
                full_floor(repo, &notes_tree)?
            }
        },
        None => full_floor(repo, &notes_tree)?,
    };

    write_cache(&cache_path, &notes_tree, &floor);
    if let Ok(mut m) = memo().lock() {
        // A long-running service sees many notes trees; keep the map small.
        if m.len() > 64 {
            m.clear();
        }
        m.insert(key, floor.clone());
    }
    Ok(floor)
}

fn write_cache(path: &std::path::Path, notes_tree: &str, floor: &[String]) {
    let Some(dir) = path.parent() else {
        return;
    };
    if !dir.is_dir() {
        return;
    }
    let cache = CacheFile {
        v: CACHE_VERSION,
        notes_tree: notes_tree.to_string(),
        floor: floor.to_vec(),
    };
    let Ok(bytes) = serde_json::to_vec(&cache) else {
        return;
    };
    let tmp = dir.join(format!("stamp_floor.json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The tree of `refs/notes/ai`, or `None` when there is no such ref.
fn notes_tree(repo: &Repository) -> Result<Option<String>, AutterError> {
    let mut args = repo.global_args_for_exec();
    args.extend(
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{AI_AUTHORSHIP_FULL_REF}^{{tree}}"),
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let out = exec_git_allow_nonzero(&args)?;
    if !out.status.success() {
        return Ok(None);
    }
    let tree = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!tree.is_empty()).then_some(tree))
}

/// A notes-tree path (`ab/cdef...` with fan-out, or flat) as a commit SHA.
fn commit_of_note_path(path: &str) -> Option<String> {
    let sha: String = path.chars().filter(|c| *c != '/').collect();
    let hex = sha.chars().all(|c| c.is_ascii_hexdigit());
    (hex && (sha.len() == 40 || sha.len() == 64)).then_some(sha)
}

/// The floor computed from every note: one `git grep` over the notes tree
/// finds the stamped ones.
fn full_floor(repo: &Repository, notes_tree: &str) -> Result<Vec<String>, AutterError> {
    let mut args = repo.global_args_for_exec();
    args.extend(
        [
            "grep",
            "--no-color",
            "-I",
            "-l",
            "-F",
            "-e",
            STAMP_PATTERN,
            notes_tree,
            "--",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let out = exec_git_allow_nonzero(&args)?;
    // `git grep` exits 1 when nothing matches.
    match out.status.code() {
        Some(0) => {}
        Some(1) => return Ok(Vec::new()),
        _ => {
            return Err(AutterError::Generic(format!(
                "git grep over the notes tree failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }
    let prefix = format!("{notes_tree}:");
    let stamped: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix(&prefix))
        .filter_map(commit_of_note_path)
        .collect();
    extend_floor(repo, Vec::new(), stamped)
}

/// The cached floor brought up to date with the notes that changed between
/// `old_tree` and `new_tree`. `None` when it has to be computed again from
/// every note: a floor commit's note changed or was removed.
fn extend_from_diff(
    repo: &Repository,
    old_tree: &str,
    new_tree: &str,
    floor: Vec<String>,
) -> Result<Option<Vec<String>>, AutterError> {
    let mut args = repo.global_args_for_exec();
    args.extend(
        [
            "diff-tree",
            "-r",
            "--no-renames",
            "--raw",
            "--no-abbrev",
            old_tree,
            new_tree,
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let out = exec_git(&args)?;
    let in_floor: HashSet<&str> = floor.iter().map(String::as_str).collect();
    // commit -> new note blob, for notes added or changed.
    let mut changed: Vec<(String, String)> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // ":<old mode> <new mode> <old oid> <new oid> <status>\t<path>"
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let (Some(new_oid), Some(status)) = (fields.get(3), fields.get(4)) else {
            continue;
        };
        let Some(commit) = commit_of_note_path(path) else {
            continue;
        };
        let added = status.starts_with('A');
        if !added && in_floor.contains(commit.as_str()) {
            return Ok(None);
        }
        if added || status.starts_with('M') {
            changed.push((commit, new_oid.to_string()));
        }
    }
    if changed.is_empty() {
        return Ok(Some(floor));
    }
    let blobs: Vec<String> = changed.iter().map(|(_, oid)| oid.clone()).collect();
    let contents = crate::git::refs::batch_read_blob_contents(repo, &blobs)?;
    let stamped: Vec<String> = changed
        .into_iter()
        .filter(|(_, oid)| {
            contents
                .get(oid)
                .is_some_and(|note| note.contains(STAMP_PATTERN))
        })
        .map(|(commit, _)| commit)
        .collect();
    extend_floor(repo, floor, stamped).map(Some)
}

/// `floor` (no member an ancestor of another) extended with the stamped
/// commits `stamped`: the result is the minimal elements of both together.
fn extend_floor(
    repo: &Repository,
    mut floor: Vec<String>,
    stamped: Vec<String>,
) -> Result<Vec<String>, AutterError> {
    let mut seen: HashSet<String> = floor.iter().cloned().collect();
    let mut pending: Vec<String> = stamped
        .into_iter()
        .filter(|c| seen.insert(c.clone()))
        .collect();
    // Only commits in this clone can be walked.
    pending = present_commits(repo, &pending)?;
    if pending.is_empty() {
        return Ok(floor);
    }
    // Oldest first, so the first one taken is usually the floor itself.
    pending = oldest_first(repo, &pending)?;
    loop {
        if !floor.is_empty() {
            let later = descendants_among(repo, &floor, &pending)?;
            pending.retain(|c| !later.contains(c));
        }
        if pending.is_empty() || floor.len() >= MAX_FLOOR {
            break;
        }
        let first = pending.remove(0);
        let later = descendants_among(repo, std::slice::from_ref(&first), &floor)?;
        floor.retain(|c| !later.contains(c));
        floor.push(first);
    }
    floor.sort();
    Ok(floor)
}

/// The commits among `commits` that are in this clone.
fn present_commits(repo: &Repository, commits: &[String]) -> Result<Vec<String>, AutterError> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = repo.global_args_for_exec();
    args.push("cat-file".to_string());
    args.push("--batch-check=%(objecttype)".to_string());
    let stdin = commits.join("\n") + "\n";
    let out = exec_git_stdin(&args, stdin.as_bytes())?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(commits
        .iter()
        .zip(text.lines())
        .filter(|(_, kind)| kind.trim() == "commit")
        .map(|(c, _)| c.clone())
        .collect())
}

/// `commits` sorted by committer date, oldest first.
fn oldest_first(repo: &Repository, commits: &[String]) -> Result<Vec<String>, AutterError> {
    let mut args = repo.global_args_for_exec();
    args.extend(
        ["rev-list", "--no-walk=sorted", "--stdin"]
            .iter()
            .map(|s| s.to_string()),
    );
    let stdin = commits.join("\n") + "\n";
    let out = exec_git_stdin(&args, stdin.as_bytes())?;
    let mut sorted: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    sorted.reverse();
    Ok(sorted)
}

/// The commits among `tips` that descend from (and are not) one of
/// `bottoms`, with one `git rev-list --ancestry-path`. `bottoms` must not
/// contain one another's ancestors: a tip that descends from one bottom and
/// is an ancestor of another would otherwise be missed.
fn descendants_among(
    repo: &Repository,
    bottoms: &[String],
    tips: &[String],
) -> Result<HashSet<String>, AutterError> {
    if bottoms.is_empty() || tips.is_empty() {
        return Ok(HashSet::new());
    }
    let mut args = repo.global_args_for_exec();
    args.extend(
        ["rev-list", "--ancestry-path", "--stdin"]
            .iter()
            .map(|s| s.to_string()),
    );
    let mut stdin = String::new();
    for tip in tips {
        stdin.push_str(tip);
        stdin.push('\n');
    }
    for bottom in bottoms {
        stdin.push('^');
        stdin.push_str(bottom);
        stdin.push('\n');
    }
    let out = exec_git_stdin(&args, stdin.as_bytes())?;
    let wanted: HashSet<&str> = tips.iter().map(String::as_str).collect();
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|c| wanted.contains(c))
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_paths_read_as_commits() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(commit_of_note_path(sha).as_deref(), Some(sha));
        assert_eq!(
            commit_of_note_path("01/23456789abcdef0123456789abcdef01234567").as_deref(),
            Some(sha)
        );
        assert_eq!(commit_of_note_path("not-a-note"), None);
        assert_eq!(commit_of_note_path("0123"), None);
    }

    #[test]
    fn the_pattern_matches_the_stamp_line_only() {
        let stamped = "x\n---\n{\n  \"schema_version\": \"authorship/3.0.0\",\n  \"integrity\": {\"v\":1}\n}";
        assert!(stamped.contains(STAMP_PATTERN));
        // An escaped mention inside a prompt does not match.
        let mention = "x\n---\n{\n  \"prompts\": {\"m\": \"\\\"integrity\\\": {\"}\n}";
        assert!(!mention.contains(STAMP_PATTERN));
    }
}
