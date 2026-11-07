//! Commits whose attribution never got written.
//!
//! A checkpoint lands in `.git/ai/working_logs/<HEAD at checkpoint time>/`.
//! When the commit is observed (the background service sees its trace2
//! events), `post_commit` turns that working log into the commit's authorship
//! note and deletes the parent's working log. If nothing observed the commit,
//! because the service was stopped, crashed, still starting, unresponsive, or
//! git's trace2 config wasn't set up, the commit still succeeds and the
//! checkpoints stay on disk under the parent sha. Nothing ever processes them
//! again, so the AI lines read as untracked.
//!
//! Those leftovers are exactly what is needed to rebuild the note later: the
//! checkpoints plus the commit's own tree. This module finds such "stranded"
//! commits on HEAD's first-parent chain and runs the same `post_commit` the
//! service would have run, using the committed file contents (not the working
//! tree, which may have moved on) as the final state.

use crate::authorship::post_commit::post_commit_with_final_state;
use crate::authorship::rebase_authorship::committed_file_snapshot_between_commits;
use crate::error::AutterError;
use crate::git::repository::{Repository, exec_git};

/// How far back along HEAD's first-parent chain to look. A stranded working
/// log older than this is left alone: rebuilding a note from checkpoints that
/// old is more likely to be wrong than useful.
pub const STRANDED_LOOKBACK_COMMITS: usize = 100;


/// A commit with no authorship note whose parent still has unprocessed
/// checkpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrandedCommit {
    pub commit: String,
    /// The parent sha the checkpoints are keyed by, or `"initial"` for a root
    /// commit.
    pub parent: String,
}

/// Outcome of [`recover_stranded_commits`].
#[derive(Debug, Default, Clone)]
pub struct StrandedRecovery {
    pub recovered: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl StrandedRecovery {
    pub fn is_empty(&self) -> bool {
        self.recovered.is_empty() && self.failed.is_empty()
    }
}

struct ChainEntry {
    commit: String,
    parent: String,
    author_email: String,
}

/// HEAD-relative first-parent chain, newest first, at most `limit` entries.
fn first_parent_chain(
    repo: &Repository,
    head: &str,
    limit: usize,
) -> Result<Vec<ChainEntry>, AutterError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "log".to_string(),
        "--first-parent".to_string(),
        "--no-show-signature".to_string(),
        format!("-n{limit}"),
        "--format=%H %P%x09%ae".to_string(),
        head.to_string(),
        "--".to_string(),
    ]);
    let output = exec_git(&args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let (shas, email) = line.split_once('\t')?;
            let mut parts = shas.split_whitespace();
            let commit = parts.next()?.to_string();
            let parent = parts.next().unwrap_or("initial").to_string();
            Some(ChainEntry {
                commit,
                parent,
                author_email: email.trim().to_lowercase(),
            })
        })
        .collect())
}

/// Full sha of the commit `rev` names (`HEAD`, a branch, a sha), or `None`
/// when it doesn't name one (an unborn branch).
fn resolve_commit_sha(repo: &Repository, rev: &str) -> Option<String> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-parse".to_string(),
        "--verify".to_string(),
        "--quiet".to_string(),
        format!("{rev}^{{commit}}"),
    ]);
    let output = exec_git(&args).ok()?;
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Whether the working log keyed by `base` still holds anything that would
/// have gone into a note.
fn working_log_has_pending_state(repo: &Repository, base: &str) -> bool {
    if !repo.storage.has_working_log(base) {
        return false;
    }
    let Ok(working_log) = repo.storage.working_log_for_base_commit(base) else {
        return false;
    };
    if !working_log.read_initial_attributions().files.is_empty() {
        return true;
    }
    working_log
        .read_all_checkpoints()
        .map(|checkpoints| !checkpoints.is_empty())
        .unwrap_or(false)
}

fn has_note(repo: &Repository, commit: &str) -> bool {
    crate::git::notes_api::read_authorship_v3(repo, commit).is_ok()
}

/// Names of the working-log directories on disk. Reading the directory is
/// cheap, and almost always it holds only HEAD's own log, which lets the common
/// case return without running git at all.
fn working_log_dirs(repo: &Repository) -> Vec<String> {
    std::fs::read_dir(&repo.storage.working_logs)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Stranded commits reachable from `head` (newest first). `head` itself counts:
/// a commit that was just made while the service was down is stranded until
/// something writes its note.
///
/// Only commits authored by this user (the repo's effective author email) are
/// considered: a commit someone else made, pulled in while the service was
/// down, must not pick up this machine's leftover checkpoints.
pub fn find_stranded_commits(
    repo: &Repository,
    head: &str,
) -> Result<Vec<StrandedCommit>, AutterError> {
    let Some(head_sha) = resolve_commit_sha(repo, head) else {
        return Ok(Vec::new());
    };
    let dirs = working_log_dirs(repo);
    // Fast path: the only working log is the one for the current HEAD (normal
    // uncommitted work), or there are none.
    if dirs.iter().all(|dir| dir == &head_sha) {
        return Ok(Vec::new());
    }

    let my_email = repo
        .effective_author_identity()
        .email
        .map(|email| email.trim().to_lowercase());

    let mut stranded = Vec::new();
    for entry in first_parent_chain(repo, &head_sha, STRANDED_LOOKBACK_COMMITS)? {
        if !dirs.iter().any(|dir| dir == &entry.parent) {
            continue;
        }
        if let Some(mine) = my_email.as_deref()
            && !mine.is_empty()
            && entry.author_email != mine
        {
            continue;
        }
        if !working_log_has_pending_state(repo, &entry.parent) {
            continue;
        }
        if has_note(repo, &entry.commit) {
            continue;
        }
        stranded.push(StrandedCommit {
            commit: entry.commit,
            parent: entry.parent,
        });
    }
    Ok(stranded)
}

/// Write the note for one stranded commit from its parent's checkpoints and
/// the commit's own tree, exactly as the background service would have.
pub fn recover_stranded_commit(
    repo: &Repository,
    stranded: &StrandedCommit,
    author: &str,
) -> Result<(), AutterError> {
    let parent = (stranded.parent != "initial").then(|| stranded.parent.clone());
    let final_state = committed_file_snapshot_between_commits(
        repo,
        parent.as_deref().or(Some("initial")),
        &stranded.commit,
    )?;
    post_commit_with_final_state(
        repo,
        parent,
        stranded.commit.clone(),
        author.to_string(),
        true,
        Some(&final_state),
    )?;
    Ok(())
}

/// Find and recover every stranded commit reachable from `head`, oldest first,
/// so a recovered commit's carried-over attributions feed the next one.
///
/// Recovering a commit that already has a child writes a working log keyed by
/// that commit which nothing will read (the child already moved on); those are
/// removed so they don't show up as stranded again.
pub fn recover_stranded_commits(
    repo: &Repository,
    head: &str,
    author: &str,
) -> Result<StrandedRecovery, AutterError> {
    let mut stranded = find_stranded_commits(repo, head)?;
    stranded.reverse();
    let head_sha = resolve_commit_sha(repo, head).unwrap_or_default();

    let mut recovery = StrandedRecovery::default();
    for (index, item) in stranded.iter().enumerate() {
        let had_own_log = repo.storage.has_working_log(&item.commit);
        match recover_stranded_commit(repo, item, author) {
            Ok(()) => {
                recovery.recovered.push(item.commit.clone());
                let next_is_stranded = stranded
                    .get(index + 1)
                    .is_some_and(|next| next.parent == item.commit);
                if !had_own_log && item.commit != head_sha && !next_is_stranded {
                    let _ = repo.storage.delete_working_log_for_base_commit(&item.commit);
                }
            }
            Err(err) => {
                tracing::debug!(commit = %item.commit, %err, "stranded commit recovery failed");
                recovery.failed.push((item.commit.clone(), err.to_string()));
            }
        }
    }
    Ok(recovery)
}

/// Short form of a sha for messages.
pub fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// Rate limit for warnings printed from hot paths (every git command, every
/// checkpoint): true at most once per `interval_secs` per `key`, tracked with a
/// stamp file under `~/.autter/internal/warnings/`. When the stamp can't be
/// read or written, warn (better repeated than silent).
pub fn should_warn_now(key: &str, interval_secs: u64) -> bool {
    let Some(dir) = crate::config::internal_dir_path().map(|dir| dir.join("warnings")) else {
        return true;
    };
    let stamp = dir.join(key);
    let recent = std::fs::metadata(&stamp)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age.as_secs() < interval_secs);
    if recent {
        return false;
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&stamp, b"");
    true
}
