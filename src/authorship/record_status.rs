//! Why a commit has (or lacks) an Autter record, and what, if anything, to do
//! about it.
//!
//! The background service writes a commit's authorship note shortly after
//! `git commit` returns. A reader that runs inside that window sees no note;
//! older commits may predate Autter in the repository; and some commits were
//! made where Autter was not recording. Each case gets its own one-line
//! explanation and at most one next step, so a plain human commit is never
//! reported as a capture failure.

use crate::authorship::labels::SUPPORTED_TOOLS_URL;
use crate::authorship::stats::CommitStats;
use crate::daemon::control_api::{ControlRequest, FamilyStatus};
use crate::git::repository::{Repository, exec_git, exec_git_stdin};
use serde::Serialize;
use std::cell::OnceCell;
use std::time::{Duration, Instant};

/// How long readers wait by default for a note the service is still writing.
pub const DEFAULT_PENDING_WAIT: Duration = Duration::from_secs(10);

/// A commit older than this can't still be waiting on the service.
const RECENT_COMMIT_WINDOW: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const DAEMON_STATUS_TIMEOUT: Duration = Duration::from_millis(500);

/// Whether Autter recorded a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RecordStatus {
    /// The commit has an authorship note.
    Recorded,
    /// The background service has unfinished work for this repository and has
    /// not written the note yet.
    Pending { pending: usize },
    /// The commit is older than the first commit Autter recorded here.
    PredatesAutter {
        /// Committer time (unix seconds) of the oldest recorded commit, or
        /// `None` when no commit here has a record yet.
        first_recorded_at: Option<i64>,
    },
    /// No note, and nothing above explains it.
    Missing { cause: MissingCause },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingCause {
    /// Checkpoints were recorded against the commit's parent, but the service
    /// never turned them into a note.
    CaptureFailed,
    /// Someone else authored the commit; their note may not be fetched.
    OtherAuthor,
    /// Made while Autter was not recording (service stopped, or another clone).
    NotCaptured,
}

impl RecordStatus {
    pub fn is_recorded(&self) -> bool {
        matches!(self, RecordStatus::Recorded)
    }

    /// Short reason, for one-line views such as `autter log`.
    pub fn short_reason(&self) -> &'static str {
        match self {
            RecordStatus::Recorded => "recorded",
            RecordStatus::Pending { .. } => "pending, the background service is still writing it",
            RecordStatus::PredatesAutter { .. } => "predates Autter in this repository",
            RecordStatus::Missing {
                cause: MissingCause::CaptureFailed,
            } => "capture failed",
            RecordStatus::Missing {
                cause: MissingCause::OtherAuthor,
            } => "made by someone else; their record is not here",
            RecordStatus::Missing {
                cause: MissingCause::NotCaptured,
            } => "made while Autter was not recording",
        }
    }
}

/// What a reader should tell the person about a commit's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    pub summary: String,
    pub next_step: Option<String>,
}

impl Explanation {
    fn new(summary: impl Into<String>, next_step: Option<&str>) -> Self {
        Self {
            summary: summary.into(),
            next_step: next_step.map(str::to_string),
        }
    }

    /// The explanation as printed text: the summary, then `Next: …` when set.
    pub fn render(&self) -> String {
        let mut out = format!("{}\n", self.summary);
        if let Some(step) = &self.next_step {
            out.push_str(&format!("Next: {step}\n"));
        }
        out
    }
}

/// Explain a commit's record. Returns `None` when there is nothing to say:
/// a recorded commit with AI or human lines needs no explanation.
pub fn explain(status: &RecordStatus, stats: &CommitStats) -> Option<Explanation> {
    explain_with_agents(status, stats, false)
}

/// [`explain`], told whether an AI tool Autter cannot capture is installed on
/// this machine. With one installed, a recorded commit with no agent lines
/// may still have been written by it, so the explanation only speaks for the
/// agents Autter captures.
pub fn explain_with_agents(
    status: &RecordStatus,
    stats: &CommitStats,
    unsupported_agent_detected: bool,
) -> Option<Explanation> {
    match status {
        RecordStatus::Recorded => {
            let nothing_attributed = stats.ai_additions == 0
                && stats.human_additions == 0
                && stats.unknown_additions > 0;
            nothing_attributed.then(|| {
                Explanation::new(
                    if unsupported_agent_detected {
                        "No AI activity detected: Autter recorded this commit, and none of the agents it captures wrote its lines."
                    } else {
                        "No AI activity detected: Autter recorded this commit, and no agent wrote its lines."
                    },
                    None,
                )
            })
        }
        RecordStatus::Pending { pending } => Some(Explanation::new(
            format!(
                "Pending: the background service is still writing this commit's record ({pending} pending)."
            ),
            Some("run the command again in a few seconds, or add --wait"),
        )),
        RecordStatus::PredatesAutter {
            first_recorded_at: Some(at),
        } => Some(Explanation::new(
            format!(
                "This commit predates Autter in this repository (first recorded commit: {}).",
                format_date(*at)
            ),
            None,
        )),
        RecordStatus::PredatesAutter {
            first_recorded_at: None,
        } => Some(Explanation::new(
            "No commit in this repository has an Autter record yet. Commits made from now on get one.",
            None,
        )),
        RecordStatus::Missing {
            cause: MissingCause::CaptureFailed,
        } => Some(Explanation::new(
            "Capture failed: checkpoints were recorded for this commit, but the background service did not write its record.",
            Some("`autter doctor`"),
        )),
        RecordStatus::Missing {
            cause: MissingCause::OtherAuthor,
        } => Some(Explanation::new(
            "No Autter record for this commit. Someone else made it; their record may not be fetched, or they don't use Autter.",
            Some("`autter fetch-notes`"),
        )),
        RecordStatus::Missing {
            cause: MissingCause::NotCaptured,
        } => Some(Explanation::new(
            "No Autter record for this commit: it was made while Autter was not recording (service stopped, or another clone or machine).",
            Some("`autter doctor`"),
        )),
    }
}

fn format_date(unix_seconds: i64) -> String {
    chrono::DateTime::from_timestamp(unix_seconds, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| unix_seconds.to_string())
}

/// Classifies commits for one repository, caching per-repository facts so a
/// view such as `autter log` can classify many commits cheaply.
pub struct RecordClassifier {
    repo: Repository,
    first_recorded_at: OnceCell<Option<i64>>,
    local_email: OnceCell<Option<String>>,
}

impl RecordClassifier {
    pub fn new(repo: &Repository) -> Self {
        Self {
            repo: repo.clone(),
            first_recorded_at: OnceCell::new(),
            local_email: OnceCell::new(),
        }
    }

    /// Classify `commit_sha`. When it has no note and the background service
    /// still has work for this repository, wait up to `wait` for the note.
    pub fn classify(&self, commit_sha: &str, wait: Duration) -> RecordStatus {
        if note_exists(&self.repo, commit_sha) {
            return RecordStatus::Recorded;
        }

        let Some(info) = commit_info(&self.repo, commit_sha) else {
            return RecordStatus::Missing {
                cause: MissingCause::NotCaptured,
            };
        };

        let parent_has_working_log = info
            .first_parent
            .as_deref()
            .is_some_and(|parent| working_log_has_checkpoints(&self.repo, parent));

        // Only a recent commit, or one whose checkpoints are still on disk,
        // can be waiting on the service; older commits never ask it.
        let committed_recently = chrono::Utc::now().timestamp() - info.committer_time
            < RECENT_COMMIT_WINDOW.as_secs() as i64;
        if parent_has_working_log || committed_recently {
            match wait_for_note(&self.repo, commit_sha, wait) {
                NoteWait::Written => return RecordStatus::Recorded,
                NoteWait::StillPending(pending) => return RecordStatus::Pending { pending },
                NoteWait::Idle => {}
            }
        }

        if parent_has_working_log {
            return RecordStatus::Missing {
                cause: MissingCause::CaptureFailed,
            };
        }

        let first_recorded_at = *self
            .first_recorded_at
            .get_or_init(|| first_recorded_commit_time(&self.repo));
        match first_recorded_at {
            None => {
                return RecordStatus::PredatesAutter {
                    first_recorded_at: None,
                };
            }
            Some(first) if info.committer_time < first => {
                return RecordStatus::PredatesAutter {
                    first_recorded_at: Some(first),
                };
            }
            Some(_) => {}
        }

        let local_email = self.local_email.get_or_init(|| {
            self.repo
                .effective_author_identity()
                .email
                .map(|email| email.to_ascii_lowercase())
        });
        let other_author = local_email
            .as_deref()
            .is_some_and(|mine| !info.author_email.eq_ignore_ascii_case(mine));
        RecordStatus::Missing {
            cause: if other_author {
                MissingCause::OtherAuthor
            } else {
                MissingCause::NotCaptured
            },
        }
    }
}

/// Classify one commit. See [`RecordClassifier::classify`].
pub fn classify_commit(repo: &Repository, commit_sha: &str, wait: Duration) -> RecordStatus {
    RecordClassifier::new(repo).classify(commit_sha, wait)
}

fn note_exists(repo: &Repository, commit_sha: &str) -> bool {
    crate::git::notes_api::read_note(repo, commit_sha).is_some()
}

enum NoteWait {
    /// The note appeared while waiting.
    Written,
    /// The service still has work queued after the wait.
    StillPending(usize),
    /// The service has nothing queued for this repository (or is unreachable).
    Idle,
}

/// While the service reports unfinished work for this repository, poll for
/// the note until it appears or `wait` runs out.
fn wait_for_note(repo: &Repository, commit_sha: &str, wait: Duration) -> NoteWait {
    let deadline = Instant::now() + wait;
    loop {
        let pending = pending_work(repo).unwrap_or(0);
        if pending == 0 {
            // The service may have finished between our note read and its
            // status reply.
            return if note_exists(repo, commit_sha) {
                NoteWait::Written
            } else {
                NoteWait::Idle
            };
        }
        if Instant::now() >= deadline {
            return NoteWait::StillPending(pending);
        }
        std::thread::sleep(POLL_INTERVAL);
        if note_exists(repo, commit_sha) {
            return NoteWait::Written;
        }
    }
}

/// Git commands and checkpoints the background service has accepted for this
/// repository and not finished. `None` when the service is unreachable.
pub fn pending_work(repo: &Repository) -> Option<usize> {
    family_status(repo).map(|status| status.pending_work())
}

/// Whether the background service answers for this repository. It is what
/// writes a commit's record, so while it is down a commit gets none.
pub fn service_is_running(repo: &Repository) -> bool {
    family_status(repo).is_some()
}

/// Checkpoints queued in the background service for this repository.
pub fn queued_checkpoints(repo: &Repository) -> Option<usize> {
    family_status(repo).map(|status| status.queued_checkpoints)
}

/// Wait up to `wait` for the service to apply queued checkpoints. Returns how
/// many are still queued (0 when the service is unreachable).
pub fn wait_for_queued_checkpoints(repo: &Repository, wait: Duration) -> usize {
    let deadline = Instant::now() + wait;
    loop {
        let queued = queued_checkpoints(repo).unwrap_or(0);
        if queued == 0 || Instant::now() >= deadline {
            return queued;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn family_status(repo: &Repository) -> Option<FamilyStatus> {
    let workdir = repo.workdir().ok()?;
    let config = crate::daemon::DaemonConfig::from_env_or_default_paths().ok()?;
    let request = ControlRequest::StatusFamily {
        repo_working_dir: workdir.display().to_string(),
    };
    let response = crate::daemon::send_control_request_with_timeout(
        &config.control_socket_path,
        &request,
        DAEMON_STATUS_TIMEOUT,
    )
    .ok()?;
    if !response.ok {
        return None;
    }
    serde_json::from_value(response.data?).ok()
}

fn working_log_has_checkpoints(repo: &Repository, base_commit: &str) -> bool {
    if !repo.storage.has_working_log(base_commit) {
        return false;
    }
    repo.storage
        .working_log_for_base_commit(base_commit)
        .and_then(|log| log.read_all_checkpoints())
        .is_ok_and(|checkpoints| !checkpoints.is_empty())
}

struct CommitInfo {
    committer_time: i64,
    author_email: String,
    first_parent: Option<String>,
}

fn commit_info(repo: &Repository, commit_sha: &str) -> Option<CommitInfo> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "log".to_string(),
        "-1".to_string(),
        "--no-walk".to_string(),
        "--format=%ct%x00%ae%x00%P".to_string(),
        commit_sha.to_string(),
        "--".to_string(),
    ]);
    let output = exec_git(&args).ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.trim_end().splitn(3, '\0');
    let committer_time = fields.next()?.trim().parse().ok()?;
    let author_email = fields.next()?.to_string();
    let first_parent = fields
        .next()
        .and_then(|parents| parents.split_whitespace().next())
        .map(str::to_string);
    Some(CommitInfo {
        committer_time,
        author_email,
        first_parent,
    })
}

/// Committer time of the oldest commit that has a note in local
/// `refs/notes/ai`. `None` when no commit here has one.
fn first_recorded_commit_time(repo: &Repository) -> Option<i64> {
    let mut list_args = repo.global_args_for_exec();
    list_args.extend([
        "notes".to_string(),
        "--ref=ai".to_string(),
        "list".to_string(),
    ]);
    let listed = exec_git(&list_args).ok()?;
    let shas: Vec<&str> = std::str::from_utf8(&listed.stdout)
        .ok()?
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .collect();
    if shas.is_empty() {
        return None;
    }

    let mut log_args = repo.global_args_for_exec();
    log_args.extend([
        "log".to_string(),
        "--no-walk=unsorted".to_string(),
        "--ignore-missing".to_string(),
        "--stdin".to_string(),
        "--format=%ct".to_string(),
    ]);
    let stdin = shas.join("\n") + "\n";
    let times = exec_git_stdin(&log_args, stdin.as_bytes()).ok()?;
    String::from_utf8_lossy(&times.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i64>().ok())
        .min()
}

/// What this machine captures, in one line, for readers that show untracked
/// lines. Reads hook configs, so call it only for interactive output.
pub fn capture_boundary_line() -> String {
    use crate::mdm::agents::get_all_installers;
    use crate::mdm::hook_installer::HookInstallerParams;

    let binary_path =
        std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("autter"));
    let params = HookInstallerParams { binary_path };
    let mut hooked = Vec::new();
    let mut unhooked = Vec::new();
    for installer in get_all_installers() {
        if let Ok(check) = installer.check_hooks(&params)
            && check.tool_installed
        {
            if check.hooks_installed {
                hooked.push(installer.name().to_string());
            } else {
                unhooked.push(installer.name().to_string());
            }
        }
    }
    format_capture_boundary(&hooked, &unhooked)
}

fn format_capture_boundary(hooked: &[String], unhooked: &[String]) -> String {
    if !hooked.is_empty() {
        let mut line = format!(
            "Captured on this machine: {}. Edits from other tools count as untracked.",
            hooked.join(", ")
        );
        if !unhooked.is_empty() {
            line.push_str(&format!(
                " Not set up: {} (run `autter install-hooks`).",
                unhooked.join(", ")
            ));
        }
        line
    } else if !unhooked.is_empty() {
        format!(
            "No agent is captured on this machine yet: {} found without hooks. Next: `autter install-hooks`",
            unhooked.join(", ")
        )
    } else {
        format!(
            "No supported agent detected on this machine. Supported tools: {SUPPORTED_TOOLS_URL}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(human: u32, untracked: u32, ai: u32) -> CommitStats {
        CommitStats {
            human_additions: human,
            unknown_additions: untracked,
            ai_additions: ai,
            ai_accepted: ai,
            git_diff_added_lines: human + untracked + ai,
            ..Default::default()
        }
    }

    #[test]
    fn recorded_commits_with_attribution_need_no_explanation() {
        assert_eq!(explain(&RecordStatus::Recorded, &stats(1, 2, 3)), None);
        assert_eq!(explain(&RecordStatus::Recorded, &stats(0, 0, 0)), None);
    }

    #[test]
    fn plain_human_commit_gets_no_troubleshooting() {
        let explanation = explain(&RecordStatus::Recorded, &stats(0, 5, 0)).unwrap();
        assert!(explanation.summary.starts_with("No AI activity detected"));
        assert_eq!(explanation.next_step, None);
        assert!(!explanation.render().contains("install-hooks"));
    }

    #[test]
    fn each_status_has_at_most_one_next_step() {
        let statuses = [
            RecordStatus::Pending { pending: 2 },
            RecordStatus::PredatesAutter {
                first_recorded_at: Some(1_767_225_600),
            },
            RecordStatus::PredatesAutter {
                first_recorded_at: None,
            },
            RecordStatus::Missing {
                cause: MissingCause::CaptureFailed,
            },
            RecordStatus::Missing {
                cause: MissingCause::OtherAuthor,
            },
            RecordStatus::Missing {
                cause: MissingCause::NotCaptured,
            },
        ];
        let rendered: Vec<String> = statuses
            .iter()
            .map(|status| explain(status, &stats(0, 5, 0)).unwrap().render())
            .collect();
        insta::assert_snapshot!(rendered.join("---\n"));
        for text in &rendered {
            assert!(text.matches("Next:").count() <= 1, "{text}");
        }
    }

    #[test]
    fn status_json_shape_is_tagged() {
        let json = serde_json::to_value(RecordStatus::Missing {
            cause: MissingCause::CaptureFailed,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"status": "missing", "cause": "capture_failed"})
        );
        let json = serde_json::to_value(RecordStatus::Recorded).unwrap();
        assert_eq!(json, serde_json::json!({"status": "recorded"}));
    }

    #[test]
    fn capture_boundary_names_hooked_tools_or_links_supported_tools() {
        let none = format_capture_boundary(&[], &[]);
        assert!(none.contains("No supported agent detected"), "{none}");
        assert!(none.contains(SUPPORTED_TOOLS_URL), "{none}");

        let unhooked = format_capture_boundary(&[], &["Cursor".to_string()]);
        assert!(
            unhooked.contains("Cursor found without hooks"),
            "{unhooked}"
        );
        assert!(unhooked.contains("autter install-hooks"), "{unhooked}");

        let hooked = format_capture_boundary(&["Claude Code".to_string()], &[]);
        assert_eq!(
            hooked,
            "Captured on this machine: Claude Code. Edits from other tools count as untracked."
        );
    }
}
