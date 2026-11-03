//! Repo-wide AI share: the number behind `autter badge`.
//!
//! The share is `sum(ai_accepted) / sum(git_diff_added_lines)` over the
//! commits in a window, the same ratio each per-commit summary carries as
//! `ai_percent` when it is uploaded to the Autter cloud (see
//! `post_commit::upload_commit_authorship_summary`). Commits without an
//! authorship note add nothing to either sum but are counted, so callers can
//! report coverage (`commits_with_authorship / commits`). Merge commits are
//! left out of the walk: they attest no lines of their own and would only
//! lower the coverage figure.

use crate::authorship::stats::stats_for_commit_stats_with_authorship;
use crate::error::AutterError;
use crate::git::notes_api::{commits_with_notes, read_authorship};
use crate::git::repository::{Repository, exec_git};
use serde::{Deserialize, Serialize};

/// Days of history the default window covers.
pub const DEFAULT_WINDOW_DAYS: i64 = 90;

/// Most commits one computation walks. Keeps `autter badge` bounded on large
/// repositories; the result says when the cap was hit.
pub const MAX_COMMITS: usize = 2000;

/// Which commits on the current branch to count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Window {
    /// Commits at or after a `git log --since` value: a date such as
    /// `2025-08-05` or a duration such as `30 days ago`.
    Since(String),
    /// Every commit reachable from HEAD.
    AllTime,
}

impl Window {
    /// The default window: the last [`DEFAULT_WINDOW_DAYS`] days.
    pub fn default_window() -> Self {
        Self::Since(default_since_date())
    }
}

/// The ISO date [`DEFAULT_WINDOW_DAYS`] days before today (UTC).
pub fn default_since_date() -> String {
    (chrono::Utc::now() - chrono::Duration::days(DEFAULT_WINDOW_DAYS))
        .format("%Y-%m-%d")
        .to_string()
}

/// The repo-wide share and the figures behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoShare {
    /// AI-accepted lines as a percentage of added lines, rounded to two
    /// decimals. Zero when no line was added in the window.
    pub ai_percent: f64,
    /// Lines accepted from an AI agent unchanged, summed over the window.
    pub ai_accepted: u64,
    /// Lines added by the counted commits (ignored files excluded).
    pub added_lines: u64,
    /// Non-merge commits in the window, including those without a note.
    pub commits: usize,
    /// Commits in the window that have an authorship note.
    pub commits_with_authorship: usize,
    /// The `--since` bound the window used, or `None` for all time.
    pub since: Option<String>,
    /// True when the walk stopped at [`MAX_COMMITS`] before the window ended.
    pub capped: bool,
}

impl RepoShare {
    /// True when there is nothing to report a share of.
    pub fn has_data(&self) -> bool {
        self.added_lines > 0
    }
}

/// Percentage `part` is of `total`, rounded to two decimals; zero when `total` is zero.
pub fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        ((part as f64 / total as f64) * 10000.0).round() / 100.0
    }
}

/// Compute the repo-wide AI share for the commits in `window` on the current
/// branch, skipping files that match `ignore_patterns`.
pub fn repo_share(
    repo: &Repository,
    window: &Window,
    ignore_patterns: &[String],
) -> Result<RepoShare, AutterError> {
    let since = match window {
        Window::Since(since) => Some(since.clone()),
        Window::AllTime => None,
    };
    let (shas, capped) = list_commits(repo, window)?;
    let with_notes = commits_with_notes(repo, &shas)?;

    let mut ai_accepted = 0u64;
    let mut added_lines = 0u64;
    let mut commits_with_authorship = 0usize;
    for sha in &shas {
        if !with_notes.contains(sha) {
            continue;
        }
        let Some(log) = read_authorship(repo, sha) else {
            continue;
        };
        let stats = stats_for_commit_stats_with_authorship(repo, sha, ignore_patterns, Some(&log))?;
        commits_with_authorship += 1;
        ai_accepted += u64::from(stats.ai_accepted);
        added_lines += u64::from(stats.git_diff_added_lines);
    }

    Ok(RepoShare {
        ai_percent: percent(ai_accepted, added_lines),
        ai_accepted,
        added_lines,
        commits: shas.len(),
        commits_with_authorship,
        since,
        capped,
    })
}

/// Non-merge commits reachable from HEAD within `window`, newest first, and
/// whether the list was cut at [`MAX_COMMITS`]. An unborn branch yields none.
fn list_commits(repo: &Repository, window: &Window) -> Result<(Vec<String>, bool), AutterError> {
    if repo.head().and_then(|head| head.target()).is_err() {
        return Ok((Vec::new(), false));
    }
    let mut args = repo.global_args_for_exec();
    args.push("log".to_string());
    args.push("--format=%H".to_string());
    args.push("--no-merges".to_string());
    args.push(format!("--max-count={}", MAX_COMMITS + 1));
    if let Window::Since(since) = window {
        args.push(format!("--since={since}"));
    }
    args.push("HEAD".to_string());
    let output = exec_git(&args)?;
    let mut shas: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let capped = shas.len() > MAX_COMMITS;
    shas.truncate(MAX_COMMITS);
    Ok((shas, capped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_rounds_to_two_decimals_and_handles_zero() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(1, 3), 33.33);
        assert_eq!(percent(2, 3), 66.67);
        assert_eq!(percent(5, 5), 100.0);
    }

    #[test]
    fn default_window_is_an_iso_date() {
        let Window::Since(date) = Window::default_window() else {
            panic!("default window must be a --since bound");
        };
        assert_eq!(date.len(), 10);
        assert!(chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_ok());
    }
}
