//! `autter stats --coverage [<rev>|<range>]`: which commits Autter recorded,
//! why the rest have no record, and whether the records reached the remote.
//!
//! Rebased, amended and cherry-picked commits get their records rewritten;
//! a rewrite that lost a record shows up here as a missing commit. Merge
//! commits add no lines of their own, so a merge without a record is listed
//! separately rather than counted as missing.

use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::record_status::{
    DEFAULT_PENDING_WAIT, MissingCause, RecordClassifier, RecordStatus,
};
use crate::error::AutterError;
use crate::git::repository::{Repository, exec_git, exec_git_allow_nonzero};
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;

/// Commits checked when no range is given.
pub const DEFAULT_COVERAGE_COMMITS: usize = 50;
/// Upper bound for an explicit range.
const MAX_COVERAGE_COMMITS: usize = 2000;

#[derive(Debug, Default, Serialize)]
pub struct CoverageReport {
    pub range: String,
    pub commits: usize,
    pub recorded: usize,
    pub recorded_with_ai: usize,
    pub recorded_without_ai: usize,
    pub pending: usize,
    pub predates_autter: usize,
    pub missing: usize,
    pub merges_without_record: usize,
    pub capped: bool,
    pub missing_commits: Vec<MissingCommit>,
    pub remote: RemoteNotes,
}

#[derive(Debug, Serialize)]
pub struct MissingCommit {
    pub sha: String,
    pub author: String,
    pub reason: String,
    pub record: RecordStatus,
}

#[derive(Debug, Default, Serialize)]
pub struct RemoteNotes {
    pub remote: Option<String>,
    /// `cloud`, `no_remote`, `not_fetched`, `in_sync` or `ahead`.
    pub state: String,
    /// Recorded commits in this report whose record isn't on the remote as of
    /// the last fetch.
    pub not_on_remote: usize,
}

pub fn run_coverage(repo: &Repository, spec: Option<&str>, json: bool) -> Result<(), AutterError> {
    let report = coverage_report(repo, spec)?;
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print!("{}", render_coverage(&report));
    }
    Ok(())
}

pub fn coverage_report(
    repo: &Repository,
    spec: Option<&str>,
) -> Result<CoverageReport, AutterError> {
    let CommitList {
        range,
        commits,
        capped,
    } = list_commits(repo, spec)?;
    let shas: Vec<String> = commits.iter().map(|(sha, _)| sha.clone()).collect();
    let notes = crate::git::notes_api::read_notes_batch(repo, &shas)?;
    let head = repo.head().and_then(|h| h.target()).ok();
    let classifier = RecordClassifier::new(repo);

    let mut report = CoverageReport {
        range,
        commits: commits.len(),
        capped,
        ..Default::default()
    };
    let mut recorded_shas = Vec::new();

    for (sha, parent_count) in &commits {
        if let Some(note) = notes.get(sha) {
            report.recorded += 1;
            recorded_shas.push(sha.clone());
            if note_has_ai_lines(note) {
                report.recorded_with_ai += 1;
            } else {
                report.recorded_without_ai += 1;
            }
            continue;
        }
        if *parent_count > 1 {
            report.merges_without_record += 1;
            continue;
        }
        let wait = if head.as_deref() == Some(sha.as_str()) {
            DEFAULT_PENDING_WAIT
        } else {
            Duration::ZERO
        };
        let record = classifier.classify(sha, wait);
        match &record {
            RecordStatus::Recorded => {
                report.recorded += 1;
                recorded_shas.push(sha.clone());
                let note = crate::git::notes_api::read_note(repo, sha).unwrap_or_default();
                if note_has_ai_lines(&note) {
                    report.recorded_with_ai += 1;
                } else {
                    report.recorded_without_ai += 1;
                }
            }
            RecordStatus::Pending { .. } => report.pending += 1,
            RecordStatus::PredatesAutter { .. } => report.predates_autter += 1,
            RecordStatus::Missing { .. } => {
                report.missing += 1;
                report.missing_commits.push(MissingCommit {
                    sha: sha.clone(),
                    author: commit_author_email(repo, sha).unwrap_or_default(),
                    reason: record.short_reason().to_string(),
                    record: record.clone(),
                });
            }
        }
    }

    report.remote = remote_notes(repo, &recorded_shas);
    Ok(report)
}

fn note_has_ai_lines(note: &str) -> bool {
    AuthorshipLog::deserialize_from_string(note).is_ok_and(|log| {
        log.attestations.iter().any(|file| {
            file.entries
                .iter()
                .any(|entry| !entry.hash.starts_with("h_"))
        })
    })
}

/// Commits to check, newest first, as `(sha, parent count)`.
struct CommitList {
    /// What was listed, for the report header.
    range: String,
    commits: Vec<(String, usize)>,
    /// More commits existed than were listed.
    capped: bool,
}

fn list_commits(repo: &Repository, spec: Option<&str>) -> Result<CommitList, AutterError> {
    let (rev, limit, description) = match spec {
        Some(range) if range.contains("..") => {
            (range.to_string(), MAX_COVERAGE_COMMITS, range.to_string())
        }
        Some(rev) => (
            rev.to_string(),
            DEFAULT_COVERAGE_COMMITS,
            format!("last {DEFAULT_COVERAGE_COMMITS} commits from {rev}"),
        ),
        None => (
            "HEAD".to_string(),
            DEFAULT_COVERAGE_COMMITS,
            format!("last {DEFAULT_COVERAGE_COMMITS} commits from HEAD"),
        ),
    };
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".to_string(),
        "--parents".to_string(),
        format!("--max-count={}", limit + 1),
        rev,
        "--".to_string(),
    ]);
    let output = exec_git(&args)?;
    let mut commits: Vec<(String, usize)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next()?.to_string();
            Some((sha, fields.count()))
        })
        .collect();
    let capped = commits.len() > limit;
    commits.truncate(limit);
    let description = if spec.is_none_or(|s| !s.contains("..")) && commits.len() < limit {
        format!(
            "all {} commits from {}",
            commits.len(),
            spec.unwrap_or("HEAD")
        )
    } else {
        description
    };
    Ok(CommitList {
        range: description,
        commits,
        capped,
    })
}

fn commit_author_email(repo: &Repository, sha: &str) -> Option<String> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "log".to_string(),
        "-1".to_string(),
        "--format=%ae".to_string(),
        sha.to_string(),
        "--".to_string(),
    ]);
    let output = exec_git(&args).ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether the records for `recorded_shas` are on the default remote, as of
/// the last notes fetch (Autter fetches notes on `git fetch`/`pull` and
/// pushes them on `git push`).
fn remote_notes(repo: &Repository, recorded_shas: &[String]) -> RemoteNotes {
    if !crate::config::Config::get()
        .notes_backend_kind()
        .uses_git_notes()
    {
        return RemoteNotes {
            remote: None,
            state: "cloud".to_string(),
            not_on_remote: 0,
        };
    }
    let Some(remote) = repo.get_default_remote().ok().flatten() else {
        return RemoteNotes {
            remote: None,
            state: "no_remote".to_string(),
            not_on_remote: 0,
        };
    };
    let tracking_ref = crate::git::refs::tracking_ref_for_remote(&remote);
    let Some(remote_annotated) = annotated_commits(repo, &tracking_ref) else {
        return RemoteNotes {
            remote: Some(remote),
            state: "not_fetched".to_string(),
            not_on_remote: 0,
        };
    };
    let not_on_remote = recorded_shas
        .iter()
        .filter(|sha| !remote_annotated.contains(*sha))
        .count();
    RemoteNotes {
        remote: Some(remote),
        state: if not_on_remote == 0 {
            "in_sync"
        } else {
            "ahead"
        }
        .to_string(),
        not_on_remote,
    }
}

/// Commits annotated in a notes ref, or `None` when the ref doesn't exist.
fn annotated_commits(repo: &Repository, notes_ref: &str) -> Option<HashSet<String>> {
    let mut verify = repo.global_args_for_exec();
    verify.extend([
        "rev-parse".to_string(),
        "--verify".to_string(),
        "--quiet".to_string(),
        notes_ref.to_string(),
    ]);
    let exists = exec_git_allow_nonzero(&verify).ok()?.status.success();
    if !exists {
        return None;
    }
    let mut list = repo.global_args_for_exec();
    list.extend([
        "notes".to_string(),
        format!("--ref={notes_ref}"),
        "list".to_string(),
    ]);
    let output = exec_git(&list).ok()?;
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
            .collect(),
    )
}

pub fn render_coverage(report: &CoverageReport) -> String {
    let mut out = format!(
        "Attribution coverage: {}{}\n",
        report.range,
        if report.capped { " (capped)" } else { "" }
    );
    let row = |count: usize, text: String| format!("  {count:>4}  {text}\n");
    out.push_str(&row(
        report.recorded,
        format!(
            "recorded ({} with AI lines, {} with no AI activity)",
            report.recorded_with_ai, report.recorded_without_ai
        ),
    ));
    if report.pending > 0 {
        out.push_str(&row(
            report.pending,
            "pending, the background service is still writing them".to_string(),
        ));
    }
    if report.predates_autter > 0 {
        out.push_str(&row(
            report.predates_autter,
            "predate Autter in this repository".to_string(),
        ));
    }
    if report.merges_without_record > 0 {
        out.push_str(&row(
            report.merges_without_record,
            "merges without a record (they add no lines of their own)".to_string(),
        ));
    }
    if report.missing > 0 {
        out.push_str(&row(report.missing, "missing a record:".to_string()));
        for commit in &report.missing_commits {
            out.push_str(&format!(
                "          {}  {}  {}\n",
                &commit.sha[..commit.sha.len().min(7)],
                commit.author,
                commit.reason
            ));
        }
    }

    let remote = report.remote.remote.as_deref().unwrap_or("remote");
    let remote_line = match report.remote.state.as_str() {
        "cloud" => "Remote: records are stored in the Autter cloud.".to_string(),
        "no_remote" => "Remote: none configured; records stay in this clone.".to_string(),
        "not_fetched" => format!("Remote ({remote}): notes not fetched yet."),
        "in_sync" => {
            format!(
                "Remote ({remote}): every recorded commit here is on {remote} as of the last fetch."
            )
        }
        _ => format!(
            "Remote ({remote}): {} recorded commit(s) here are not on {remote} as of the last fetch; `git push` shares them.",
            report.remote.not_on_remote
        ),
    };
    out.push_str(&remote_line);
    out.push('\n');

    if let Some(step) = next_step(report) {
        out.push_str(&format!("Next: {step}\n"));
    }
    out
}

/// At most one thing to do, most useful first.
fn next_step(report: &CoverageReport) -> Option<&'static str> {
    let cause = |wanted: MissingCause| {
        report.missing_commits.iter().any(
            |commit| matches!(commit.record, RecordStatus::Missing { cause } if cause == wanted),
        )
    };
    if cause(MissingCause::CaptureFailed) || cause(MissingCause::NotCaptured) {
        Some("`autter doctor`")
    } else if cause(MissingCause::OtherAuthor) || report.remote.state == "not_fetched" {
        Some("`autter fetch-notes`")
    } else if report.pending > 0 {
        Some("run the command again in a few seconds")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_report_renders_each_bucket_and_one_next_step() {
        let report = CoverageReport {
            range: "last 50 commits from HEAD".to_string(),
            commits: 9,
            recorded: 5,
            recorded_with_ai: 3,
            recorded_without_ai: 2,
            pending: 1,
            predates_autter: 1,
            missing: 1,
            merges_without_record: 1,
            capped: false,
            missing_commits: vec![MissingCommit {
                sha: "e4f5a6b7c8".to_string(),
                author: "alice@example.com".to_string(),
                reason: "made by someone else; their record is not here".to_string(),
                record: RecordStatus::Missing {
                    cause: MissingCause::OtherAuthor,
                },
            }],
            remote: RemoteNotes {
                remote: Some("origin".to_string()),
                state: "ahead".to_string(),
                not_on_remote: 2,
            },
        };
        insta::assert_snapshot!(render_coverage(&report));
    }

    #[test]
    fn fully_recorded_history_needs_no_next_step() {
        let report = CoverageReport {
            range: "all 3 commits from HEAD".to_string(),
            commits: 3,
            recorded: 3,
            recorded_without_ai: 3,
            remote: RemoteNotes {
                remote: None,
                state: "no_remote".to_string(),
                not_on_remote: 0,
            },
            ..Default::default()
        };
        let text = render_coverage(&report);
        assert!(!text.contains("Next:"), "{text}");
    }
}
