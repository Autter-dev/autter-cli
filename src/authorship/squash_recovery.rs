//! Automatic recovery of authorship notes for remotely squash-merged branches.
//!
//! When a PR is squash-merged on the hosting provider (GitHub/GitLab), the
//! squash commit is created server-side: no local hook runs, so no authorship
//! note is written for it and `autter blame` degrades every line it introduced
//! to "untracked" — even though the source branch's commits still carry full
//! attribution locally. `autter squash-authorship` recovers this manually;
//! this module runs the same recovery automatically after `git pull`.
//!
//! Detection: a pulled commit is considered the squash of a local branch when
//! the branch is unmerged, has authorship notes, shares a merge base with the
//! commit's parent, and either the branch tip's tree equals the commit's tree
//! (branch was up to date with the base when merged) or the two diffs have the
//! same `git patch-id --stable` (base advanced, but the squash applied the
//! same content change). Patch equivalence is the same signal `git cherry`
//! uses, so a matched commit introduces byte-identical changes and reusing the
//! branch's attribution for it is content-accurate.

use std::collections::HashMap;

use crate::authorship::rebase_authorship::rewrite_authorship_after_squash_or_rebase;
use crate::error::AutterError;
use crate::git::authorship_traversal::commits_have_authorship_notes;
use crate::git::notes_api::read_notes_batch;
use crate::git::repository::{Repository, exec_git, exec_git_stdin};

/// Bounds keeping the post-pull scan cheap on huge pulls and branchy repos.
/// When a cap truncates, the skipped items simply stay unrecovered (same as
/// before this feature); `autter squash-authorship` remains the manual path.
const MAX_NEW_COMMITS: usize = 50;
const MAX_CANDIDATE_BRANCHES: usize = 100;
const MAX_SOURCE_COMMITS: usize = 400;

struct CandidateBranch {
    refname_short: String,
    tip: String,
    tip_tree: String,
    /// merge-base -> whether merge_base..tip carries any authorship notes
    has_notes_by_base: HashMap<String, bool>,
    /// merge-base -> patch id of diff(merge_base, tip)
    patch_id_by_base: HashMap<String, Option<String>>,
}

impl CandidateBranch {
    fn has_notes(&mut self, repo: &Repository, merge_base: &str) -> bool {
        if let Some(known) = self.has_notes_by_base.get(merge_base) {
            return *known;
        }
        let source_commits = rev_list(repo, merge_base, &self.tip);
        let result = if source_commits.is_empty() || source_commits.len() > MAX_SOURCE_COMMITS {
            false
        } else {
            commits_have_authorship_notes(repo, &source_commits).unwrap_or(false)
        };
        self.has_notes_by_base
            .insert(merge_base.to_string(), result);
        result
    }

    fn patch_id(&mut self, repo: &Repository, merge_base: &str) -> Option<String> {
        if let Some(known) = self.patch_id_by_base.get(merge_base) {
            return known.clone();
        }
        let result = patch_id_for_diff(repo, merge_base, &self.tip);
        self.patch_id_by_base
            .insert(merge_base.to_string(), result.clone());
        result
    }
}

/// After a pull advanced `old_head` to `new_head`, reconstruct authorship
/// notes for pulled commits that are squash merges of local branches.
/// Returns the number of commits that had a note reconstructed.
///
/// Best-effort by design: every per-commit and per-branch failure downgrades
/// to "no recovery for that pair", never to an error that could disturb pull
/// processing. Runs after remote notes have been fetched, so commits whose
/// notes already exist (e.g. written by `autter ci`) are left untouched.
pub fn recover_squashed_authorship_after_pull(
    repo: &Repository,
    old_head: &str,
    new_head: &str,
) -> Result<usize, AutterError> {
    if old_head.is_empty() || new_head.is_empty() || old_head == new_head {
        return Ok(0);
    }

    let new_commits = pulled_single_parent_commits_without_notes(repo, old_head, new_head)?;
    if new_commits.is_empty() {
        return Ok(0);
    }

    let mut candidates = candidate_local_branches(repo, new_head)?;
    if candidates.is_empty() {
        return Ok(0);
    }

    // Only used by the rewrite for multi-parent merge commits, which are
    // filtered out above; resolved anyway so the call sites stay honest.
    let merge_ref = repo
        .head()
        .ok()
        .and_then(|head| head.name().map(|name| name.to_string()))
        .unwrap_or_default();

    let mut recovered = 0usize;
    for (commit_sha, parent_sha) in &new_commits {
        let Some(index) = find_matching_candidate(repo, commit_sha, parent_sha, &mut candidates)
        else {
            continue;
        };
        // A branch squashes into exactly one commit; never match it twice.
        let candidate = candidates.remove(index);
        match rewrite_authorship_after_squash_or_rebase(
            repo,
            "",
            &merge_ref,
            &candidate.tip,
            commit_sha,
            true,
        ) {
            Ok(()) => {
                recovered += 1;
                tracing::info!(
                    branch = %candidate.refname_short,
                    source_head = %candidate.tip,
                    squash_commit = %commit_sha,
                    "recovered authorship note for remotely squash-merged branch"
                );
            }
            Err(error) => {
                tracing::debug!(
                    branch = %candidate.refname_short,
                    squash_commit = %commit_sha,
                    %error,
                    "failed to recover authorship for squash-merged branch"
                );
            }
        }
        if candidates.is_empty() {
            break;
        }
    }

    Ok(recovered)
}

/// Commits introduced by the pull (`old_head..new_head`, oldest first) that
/// have exactly one parent and no authorship note yet. Merge commits keep
/// their sources in the ancestry, so attribution survives without help;
/// commits that already have a note (fetched from the remote, or produced by
/// the rebase rewrite for `pull --rebase`) need none either.
fn pulled_single_parent_commits_without_notes(
    repo: &Repository,
    old_head: &str,
    new_head: &str,
) -> Result<Vec<(String, String)>, AutterError> {
    let mut args = repo.global_args_for_exec();
    args.push("rev-list".to_string());
    args.push("--reverse".to_string());
    args.push("--parents".to_string());
    args.push(format!("{}..{}", old_head, new_head));
    // A pull that leaves unrelated/unknown oids behind is not recoverable —
    // treat rev-list failure as "nothing new" rather than an error.
    let Ok(output) = exec_git(&args) else {
        return Ok(Vec::new());
    };

    let stdout = String::from_utf8(output.stdout)?;
    let mut single_parent: Vec<(String, String)> = stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next()?.to_string();
            let parent = fields.next()?.to_string();
            // A second parent means a merge commit — skip.
            fields.next().is_none().then_some((sha, parent))
        })
        .collect();

    if single_parent.len() > MAX_NEW_COMMITS {
        tracing::debug!(
            total = single_parent.len(),
            kept = MAX_NEW_COMMITS,
            "squash recovery: large pull, only scanning the newest commits"
        );
        single_parent = single_parent.split_off(single_parent.len() - MAX_NEW_COMMITS);
    }

    let shas: Vec<String> = single_parent.iter().map(|(sha, _)| sha.clone()).collect();
    let existing_notes = read_notes_batch(repo, &shas).unwrap_or_default();
    Ok(single_parent
        .into_iter()
        .filter(|(sha, _)| !existing_notes.contains_key(sha))
        .collect())
}

/// Local branches whose tips are not reachable from `new_head`. Branches that
/// were merged normally (or fast-forwarded) are ancestors of the pulled head
/// and need no recovery, and `--no-merged` also excludes the pulled branch
/// itself. Tips and tip trees come from the same single `for-each-ref` call.
fn candidate_local_branches(
    repo: &Repository,
    new_head: &str,
) -> Result<Vec<CandidateBranch>, AutterError> {
    let mut args = repo.global_args_for_exec();
    args.push("for-each-ref".to_string());
    args.push("--format=%(refname:short)%00%(objectname)%00%(tree)".to_string());
    args.push(format!("--no-merged={}", new_head));
    args.push(format!("--count={}", MAX_CANDIDATE_BRANCHES));
    args.push("refs/heads".to_string());
    let output = exec_git(&args)?;

    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let refname_short = fields.next()?.to_string();
            let tip = fields.next()?.to_string();
            let tip_tree = fields.next()?.to_string();
            if refname_short.is_empty() || tip.is_empty() || tip_tree.is_empty() {
                return None;
            }
            Some(CandidateBranch {
                refname_short,
                tip,
                tip_tree,
                has_notes_by_base: HashMap::new(),
                patch_id_by_base: HashMap::new(),
            })
        })
        .collect())
}

/// Find the candidate branch that `commit_sha` is a squash merge of, if any.
fn find_matching_candidate(
    repo: &Repository,
    commit_sha: &str,
    parent_sha: &str,
    candidates: &mut [CandidateBranch],
) -> Option<usize> {
    let commit_tree = repo
        .find_commit(commit_sha.to_string())
        .ok()?
        .tree()
        .ok()?
        .id();
    let mut commit_patch_id: Option<Option<String>> = None;

    for (index, candidate) in candidates.iter_mut().enumerate() {
        let Ok(Some(merge_base)) = repo.merge_base(candidate.tip.clone(), parent_sha.to_string())
        else {
            continue;
        };
        // Branch tip reachable from the commit's parent: already in history.
        if merge_base == candidate.tip {
            continue;
        }
        // Without notes on the branch there is nothing to recover, and any
        // tree/patch match would only manufacture an empty note.
        if !candidate.has_notes(repo, &merge_base) {
            continue;
        }

        // Fast path: squashing an up-to-date branch reproduces its tip tree.
        if candidate.tip_tree == commit_tree {
            return Some(index);
        }

        // Slow path: the base advanced, but the squash landed the same change.
        let commit_pid = commit_patch_id
            .get_or_insert_with(|| patch_id_for_diff(repo, parent_sha, commit_sha))
            .clone();
        let Some(commit_pid) = commit_pid else {
            // No diff to compare (e.g. empty squash) — no commit matches.
            return None;
        };
        if candidate.patch_id(repo, &merge_base) == Some(commit_pid) {
            return Some(index);
        }
    }

    None
}

/// `git patch-id --stable` of `git diff <base> <head>`, or None when the diff
/// is empty or either step fails.
fn patch_id_for_diff(repo: &Repository, base: &str, head: &str) -> Option<String> {
    let mut diff_args = repo.global_args_for_exec();
    diff_args.push("diff".to_string());
    diff_args.push(base.to_string());
    diff_args.push(head.to_string());
    let diff = exec_git(&diff_args).ok()?;
    if diff.stdout.is_empty() {
        return None;
    }

    let mut patch_id_args = repo.global_args_for_exec();
    patch_id_args.push("patch-id".to_string());
    patch_id_args.push("--stable".to_string());
    let output = exec_git_stdin(&patch_id_args, &diff.stdout).ok()?;
    String::from_utf8(output.stdout)
        .ok()?
        .split_whitespace()
        .next()
        .map(|id| id.to_string())
}

/// Commit shas in `base..head`, empty on any failure.
fn rev_list(repo: &Repository, base: &str, head: &str) -> Vec<String> {
    let mut args = repo.global_args_for_exec();
    args.push("rev-list".to_string());
    args.push(format!("{}..{}", base, head));
    let Ok(output) = exec_git(&args) else {
        return Vec::new();
    };
    String::from_utf8(output.stdout)
        .map(|stdout| {
            stdout
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect()
        })
        .unwrap_or_default()
}
