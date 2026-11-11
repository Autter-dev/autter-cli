use crate::authorship::ignore::{
    IgnoreMatcher, build_ignore_matcher, effective_ignore_patterns, should_ignore_file_with_matcher,
};
use crate::authorship::stats::{
    CaptureStatus, CommitStats, StatsSummary, stats_from_authorship_log, write_stats_to_terminal,
};
use crate::authorship::virtual_attribution::VirtualAttributions;
use crate::authorship::working_log::CheckpointKind;
use crate::commands::output::{self, ErrorKind};
use crate::error::AutterError;
use crate::git::find_repository;
use crate::git::repo_storage::InitialAttributions;
use crate::git::repository::{InternalGitProfile, Repository, exec_git_with_profile};
use crate::git::status::MAX_PATHSPEC_ARGS;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::io::IsTerminal;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
struct CheckpointInfo {
    time_ago: String,
    additions: u32,
    deletions: u32,
    tool_model: String,
    is_human: bool,
}

/// `autter status --json`. `lines`/`percent`/`capture` describe the
/// uncommitted changes in the files checkpoints touched (with no checkpoints:
/// every change to a tracked file, all Unknown).
#[derive(Serialize)]
struct StatusOutput {
    head: String,
    stats: CommitStats,
    #[serde(flatten)]
    summary: StatsSummary,
    checkpoints: Vec<CheckpointInfo>,
    /// Checkpoints the background service has received but not yet written.
    pending_checkpoints: usize,
    cloud_sync: crate::auth::notice::CloudSyncStatusReport,
}

pub fn handle_status(args: &[String]) {
    use crate::commands::arg_parser::{self, ScanMode};

    let pp = match arg_parser::pre_parse(args, ScanMode::Full, false) {
        Ok(p) => p,
        Err(e) => output::fail(ErrorKind::Usage, format!("error: {}", e)),
    };
    if pp.flags.help {
        arg_parser::print_command_help("status");
        return;
    }
    arg_parser::merge_global_flags(&pp.flags);
    if let Some(arg) = pp.rest.first() {
        arg_parser::exit_unknown_option("status", arg);
    }

    let repo = match find_repository(&[]) {
        Ok(repo) => repo,
        Err(e) => crate::commands::exit_repository_error("status", &e),
    };
    match run_status(&repo, arg_parser::json(), output::strict()) {
        Ok(()) => {}
        Err(_) if crate::commands::repository_has_no_commits(&repo) => output::fail(
            ErrorKind::Runtime,
            crate::commands::no_commits_message("status"),
        ),
        Err(e) => output::fail(ErrorKind::Runtime, format!("Error: {}", e)),
    }
}

/// The `--strict` failure for `status`.
const STATUS_NOTHING_CAPTURED: &str = "nothing captured: there are uncommitted changes, and no added line is attributed to AI or a known human (--strict). Run `autter doctor` to check capture.";

fn run_status(repo: &Repository, json: bool, strict: bool) -> Result<(), AutterError> {
    let repo = repo.clone();
    let ignore_patterns = effective_ignore_patterns(&repo, &[], &[]);
    let ignore_matcher = build_ignore_matcher(&ignore_patterns);

    let default_user_name = repo.effective_author_identity().formatted_or_unknown();

    let head = repo.head()?;
    let head_sha = head.target()?;

    // A checkpoint an agent just sent may still be queued in the background
    // service; give it a moment to land before reading the working log.
    let pending_checkpoints = crate::authorship::record_status::wait_for_queued_checkpoints(
        &repo,
        PENDING_CHECKPOINT_WAIT,
    );

    let working_log = repo.storage.working_log_for_base_commit(&head_sha)?;
    let checkpoints = working_log.read_all_checkpoints()?;
    let initial_attributions = working_log.read_initial_attributions();

    let has_checkpoints = !checkpoints.is_empty();
    let has_initial = !initial_attributions.files.is_empty();

    if !has_checkpoints && !has_initial {
        // Nothing was checkpointed: every uncommitted change to a tracked file
        // is Unknown. Only the line counts are needed, for --json and --strict.
        let (added, deleted) = if json || strict {
            get_working_dir_diff_stats(&repo, None, &ignore_matcher)?
        } else {
            (0, 0)
        };
        let stats = stats_from_authorship_log(None, added, deleted, 0, 0, &BTreeMap::new());
        let summary = StatsSummary::from_stats(&stats, false);
        let nothing_captured = strict && summary.capture.status == CaptureStatus::NothingCaptured;
        if json {
            let report = StatusOutput {
                head: head_sha.clone(),
                stats,
                summary,
                checkpoints: vec![],
                pending_checkpoints,
                cloud_sync: crate::auth::notice::collect_cloud_sync_status(),
            };
            if nothing_captured {
                output::fail_with(
                    ErrorKind::NothingCaptured,
                    STATUS_NOTHING_CAPTURED,
                    Some(&report),
                );
            }
            output::print_json(&report);
        } else if !crate::commands::arg_parser::quiet() {
            eprint!("{}", no_checkpoints_message(&head_sha, pending_checkpoints));
            if pending_checkpoints == 0 && std::io::stderr().is_terminal() {
                eprintln!(
                    "{}",
                    crate::authorship::record_status::capture_boundary_line()
                );
            }
        }
        if !json
            && !crate::commands::arg_parser::quiet()
            && let Some(line) = crate::auth::notice::format_cloud_sync_status_line()
        {
            eprintln!("{line}");
        }
        if nothing_captured {
            output::fail(ErrorKind::NothingCaptured, STATUS_NOTHING_CAPTURED);
        }
        return Ok(());
    }

    let mut checkpoint_infos = Vec::new();

    for checkpoint in checkpoints.iter().rev() {
        let (additions, deletions) = (
            checkpoint.line_stats.additions,
            checkpoint.line_stats.deletions,
        );

        let tool_model = checkpoint
            .agent_id
            .as_ref()
            .map(|a| format!("{} {}", capitalize(&a.tool), &a.model))
            .unwrap_or_else(|| default_user_name.clone());

        let is_human = checkpoint.kind == CheckpointKind::Human;
        checkpoint_infos.push(CheckpointInfo {
            time_ago: format_time_ago(checkpoint.timestamp),
            additions,
            deletions,
            tool_model,
            is_human,
        });
    }

    let working_va = VirtualAttributions::from_just_working_log(
        repo.clone(),
        head_sha.clone(),
        Some(default_user_name.clone()),
    )?;

    let mut pathspecs: HashSet<String> = checkpoints
        .iter()
        .flat_map(|cp| cp.entries.iter().map(|e| e.file.clone()))
        .filter(|file| !should_ignore_file_with_matcher(file, &ignore_matcher))
        .collect();
    for file_path in working_va.files() {
        if !should_ignore_file_with_matcher(&file_path, &ignore_matcher) {
            pathspecs.insert(file_path);
        }
    }

    let (authorship_log, initial) = working_va.to_authorship_log_and_initial_working_log(
        &repo,
        &head_sha,
        &head_sha,
        Some(&pathspecs),
        None,
    )?;

    // Get actual git diff stats between HEAD and working directory (like post_commit does)
    let (total_additions, total_deletions) =
        get_working_dir_diff_stats(&repo, Some(&pathspecs), &ignore_matcher)?;

    // For status (uncommitted changes), the AI attributions are in `initial` (uncommitted),
    // not in authorship_log.attestations (which is for committed changes).
    // Count AI lines from the uncommitted attributions.
    let (ai_accepted, known_human) = count_lines_from_initial(&initial, &ignore_matcher);

    let stats = stats_from_authorship_log(
        Some(&authorship_log),
        total_additions,
        total_deletions,
        ai_accepted,
        known_human,
        &BTreeMap::new(),
    );
    let summary = StatsSummary::from_stats(&stats, true);
    let nothing_captured = strict && summary.capture.status == CaptureStatus::NothingCaptured;

    if json {
        let report = StatusOutput {
            head: head_sha.clone(),
            stats,
            summary,
            checkpoints: checkpoint_infos,
            pending_checkpoints,
            cloud_sync: crate::auth::notice::collect_cloud_sync_status(),
        };
        if nothing_captured {
            output::fail_with(
                ErrorKind::NothingCaptured,
                STATUS_NOTHING_CAPTURED,
                Some(&report),
            );
        }
        output::print_json(&report);
        return Ok(());
    }

    if crate::commands::arg_parser::quiet() {
        if nothing_captured {
            output::fail(ErrorKind::NothingCaptured, STATUS_NOTHING_CAPTURED);
        }
        return Ok(());
    }
    write_stats_to_terminal(&stats, true);
    if pending_checkpoints > 0 {
        println!("{}", pending_line(pending_checkpoints));
    }
    println!("Changes captured on this computer. Commit them to record committed-code totals.");
    if let Some(line) = crate::auth::notice::format_cloud_sync_status_line() {
        println!("{line}");
    }

    println!();
    for cp in &checkpoint_infos {
        let add_str = if cp.additions > 0 {
            format!("+{}", cp.additions)
        } else {
            "0".to_string()
        };
        let del_str = if cp.deletions > 0 {
            format!("-{}", cp.deletions)
        } else {
            "0".to_string()
        };

        let line = format!(
            "{:<14} {:>5}  {:>5}  {}",
            cp.time_ago, add_str, del_str, cp.tool_model
        );

        if cp.is_human && crate::commands::arg_parser::use_color() {
            println!("\x1b[90m{}\x1b[0m", line);
        } else {
            println!("{}", line);
        }
    }

    if nothing_captured {
        output::fail(ErrorKind::NothingCaptured, STATUS_NOTHING_CAPTURED);
    }
    Ok(())
}

fn format_time_ago(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let diff = now.saturating_sub(timestamp);

    if diff < 60 {
        format!("{} secs ago", diff)
    } else if diff < 3600 {
        format!("{} mins ago", diff / 60)
    } else if diff < 86400 {
        format!("{} hours ago", diff / 3600)
    } else {
        format!("{} days ago", diff / 86400)
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

/// Get git diff statistics between HEAD and the working directory
/// This mirrors the logic in stats.rs get_git_diff_stats but for uncommitted changes
fn get_working_dir_diff_stats(
    repo: &Repository,
    pathspecs: Option<&HashSet<String>>,
    ignore_matcher: &IgnoreMatcher,
) -> Result<(u32, u32), AutterError> {
    let mut args = repo.global_args_for_exec();
    args.push("diff".to_string());
    args.push("--numstat".to_string());
    args.push("HEAD".to_string());

    // Add pathspecs if provided to scope the diff to specific files
    // Only pass as CLI args when under threshold to avoid E2BIG
    let needs_post_filter = if let Some(paths) = pathspecs {
        if paths.is_empty() {
            return Ok((0, 0));
        }
        if paths.len() > MAX_PATHSPEC_ARGS {
            // Disable rename detection so git reports renames as separate
            // delete + add entries with clean filenames. Without this,
            // numstat outputs "old => new" arrow notation in the filename
            // field, which won't match pathspec entries.
            args.push("--no-renames".to_string());
            true
        } else {
            args.push("--".to_string());
            for path in paths {
                args.push(path.clone());
            }
            false
        }
    } else {
        false
    };

    let output = exec_git_with_profile(&args, InternalGitProfile::NumstatParse)?;
    let stdout = String::from_utf8(output.stdout)?;

    let mut added_lines = 0u32;
    let mut deleted_lines = 0u32;

    // Parse numstat output
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        // Parse numstat format: "added\tdeleted\tfilename"
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            // Post-filter by pathspec when we couldn't pass them as CLI args
            if needs_post_filter
                && let Some(paths) = pathspecs
                && !paths.contains(parts[2])
            {
                continue;
            }

            let file_path = parts[2];
            if should_ignore_file_with_matcher(file_path, ignore_matcher) {
                continue;
            }

            // Parse added lines
            if let Ok(added) = parts[0].parse::<u32>() {
                added_lines += added;
            }

            // Parse deleted lines (handle "-" for binary files)
            if parts[1] != "-"
                && let Ok(deleted) = parts[1].parse::<u32>()
            {
                deleted_lines += deleted;
            }
        }
    }

    Ok((added_lines, deleted_lines))
}

/// Count AI-attributed lines from InitialAttributions (uncommitted changes)
/// Uncommitted (AI, known-human) line counts from the working log's
/// carried attributions.
fn count_lines_from_initial(
    initial: &InitialAttributions,
    ignore_matcher: &IgnoreMatcher,
) -> (u32, u32) {
    let mut ai_lines = 0u32;
    let mut human_lines = 0u32;

    for (file_path, line_attrs) in &initial.files {
        if should_ignore_file_with_matcher(file_path, ignore_matcher) {
            continue;
        }

        for line_attr in line_attrs {
            let is_ai = if line_attr.author_id.starts_with("s_") {
                let session_key = line_attr
                    .author_id
                    .split("::")
                    .next()
                    .unwrap_or(&line_attr.author_id);
                initial.sessions.contains_key(session_key)
            } else {
                initial.prompts.contains_key(&line_attr.author_id)
            };
            let lines_count = line_attr.end_line - line_attr.start_line + 1;
            if is_ai {
                ai_lines += lines_count;
            } else if line_attr.author_id.starts_with("h_") {
                human_lines += lines_count;
            }
        }
    }

    (ai_lines, human_lines)
}

/// How long `autter status` waits for checkpoints still queued in the
/// background service.
const PENDING_CHECKPOINT_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

fn pending_line(pending: usize) -> String {
    let noun = if pending == 1 {
        "checkpoint is"
    } else {
        "checkpoints are"
    };
    format!(
        "{pending} {noun} still being written by the background service; run `autter status` again in a moment."
    )
}

/// Shown when nothing has been checkpointed since HEAD. A quiet working tree
/// is normal, so this suggests nothing unless checkpoints are still pending.
fn no_checkpoints_message(head_sha: &str, pending: usize) -> String {
    let short = &head_sha[..head_sha.len().min(7)];
    if pending > 0 {
        format!(
            "No checkpoints written since the last commit ({short}) yet.\n{}\n",
            pending_line(pending)
        )
    } else {
        format!("No AI activity recorded since the last commit ({short}).\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_working_tree_gets_no_install_advice() {
        let message = no_checkpoints_message("abcdef1234", 0);
        assert_eq!(
            message,
            "No AI activity recorded since the last commit (abcdef1).\n"
        );
    }

    #[test]
    fn queued_checkpoints_are_reported_as_pending() {
        let message = no_checkpoints_message("abcdef1234", 2);
        assert!(
            message.contains("2 checkpoints are still being written"),
            "{message}"
        );
        assert!(!message.contains("install-hooks"), "{message}");
        assert_eq!(
            pending_line(1),
            "1 checkpoint is still being written by the background service; run `autter status` again in a moment."
        );
    }
}
