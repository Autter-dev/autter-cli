use crate::authorship::authorship_log::LineRange;
use crate::authorship::ignore::{build_ignore_matcher, should_ignore_file_with_matcher};
use crate::authorship::labels::{AI_SHARE_MEANING, LineCategory};
use crate::authorship::record_status::RecordStatus;
use crate::error::AutterError;
use crate::git::notes_api::read_authorship as get_authorship;
use crate::git::repository::Repository;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::IsTerminal;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolModelHeadlineStats {
    #[serde(default)]
    pub ai_additions: u32, // Number of lines committed with AI attribution
    #[serde(default)]
    pub ai_accepted: u32, // Number of AI-generated lines that were accepted by the user without any human edits
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitStats {
    #[serde(default)]
    pub human_additions: u32, // Number of lines committed with human attribution
    #[serde(default)]
    pub unknown_additions: u32, // Number of lines with no attestation at all
    #[serde(default)]
    pub ai_additions: u32, // Number of lines committed with AI attribution
    #[serde(default)]
    pub ai_accepted: u32, // Number of AI-generated lines that were accepted by the user without any human edits
    #[serde(default)]
    pub git_diff_deleted_lines: u32,
    #[serde(default)]
    pub git_diff_added_lines: u32,
    #[serde(default)]
    pub tool_model_breakdown: BTreeMap<String, ToolModelHeadlineStats>,
}

pub fn stats_command(
    repo: &Repository,
    commit_sha: Option<&str>,
    json: bool,
    ignore_patterns: &[String],
) -> Result<(), AutterError> {
    let report = commit_stats_report(repo, commit_sha, ignore_patterns)?;
    print_commit_stats_report(&report, json);
    Ok(())
}

/// The `autter stats` report for one commit: the raw [`CommitStats`] plus the
/// line breakdown, percentages and capture status that `--json` and
/// `--strict` use.
pub fn commit_stats_report(
    repo: &Repository,
    commit_sha: Option<&str>,
    ignore_patterns: &[String],
) -> Result<CommitStatsReport, AutterError> {
    let (target, refname) = if let Some(sha) = commit_sha {
        // Validate that the commit exists using revparse_single
        match repo.revparse_single(sha) {
            Ok(commit_obj) => {
                // For a specific commit, we don't have a refname, so use the commit SHA
                let full_sha = commit_obj.id();
                (full_sha, sha.to_string())
            }
            Err(AutterError::GitCliError { .. }) => {
                return Err(AutterError::Generic(format!("No commit found: {}", sha)));
            }
            Err(e) => return Err(e),
        }
    } else {
        // Default behavior: use current HEAD
        let head = repo.head()?;
        let target = head.target()?;
        let name = head.name().unwrap_or("HEAD").to_string();
        (target, name)
    };

    tracing::debug!(
        "Stats command found commit: {} refname: {}",
        target,
        refname
    );

    // Right after `git commit` the background service may still be writing
    // the note; wait for it rather than report every line as untracked.
    let record = crate::authorship::record_status::classify_commit(
        repo,
        &target,
        crate::authorship::record_status::DEFAULT_PENDING_WAIT,
    );

    let stats = stats_for_commit_stats(repo, &target, ignore_patterns)?;
    Ok(CommitStatsReport::with_record(target, stats, record))
}

/// Print a commit's stats: the JSON document with `--json`, the bar otherwise.
pub fn print_commit_stats_report(report: &CommitStatsReport, json: bool) {
    if json {
        crate::commands::output::print_json(report);
        return;
    }
    let stats = &report.stats;
    let Some(record) = report.record.as_ref() else {
        write_stats_to_terminal(stats, true);
        return;
    };

    let unattributed = if record.is_recorded() {
        LineCategory::Untracked
    } else {
        LineCategory::Unknown
    };
    print!(
        "{}",
        render_stats_output(
            stats,
            crate::commands::arg_parser::use_color(),
            unattributed,
        )
    );
    // The unsupported-agent notice is cheap and explains the untracked share,
    // so it is printed in piped output too. It is worked out before the
    // footer: with such an agent installed, the footer must not say that no
    // agent wrote the lines.
    let notice = if record.is_recorded() {
        unsupported_agent_notice_for(stats, stats.unknown_additions)
    } else {
        None
    };
    print!("{}", stats_footer(stats, record, notice.is_some()));

    if stats.unknown_additions > 0 && record.is_recorded() {
        // Probing every agent installer is slow, so only on a terminal.
        if std::io::stdout().is_terminal() {
            println!(
                "{}",
                crate::authorship::record_status::capture_boundary_line()
            );
        }
        if let Some(notice) = notice {
            print!("{notice}");
        }
    }
}

/// Added lines under the labels `stats` and `blame` show: AI, Human (a known
/// human typed it), Untracked (the commit has an authorship note, but nothing
/// attested these lines) and Unknown (no authorship note at all, so autter
/// has no data). Every added line is in exactly one of the four.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct LineBreakdown {
    pub added: u32,
    pub deleted: u32,
    pub ai: u32,
    pub human: u32,
    pub untracked: u32,
    pub unknown: u32,
}

/// Each label's share of the added lines, in percent with one decimal.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct PercentBreakdown {
    pub ai: f64,
    pub human: f64,
    pub untracked: f64,
    pub unknown: f64,
}

/// Whether autter captured anything for what a command looked at.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStatus {
    /// Autter has a record for at least one added line: the line is AI,
    /// human, or untracked (recorded, with no agent or editor extension
    /// behind it). A commit typed entirely by hand while capture was working
    /// is `Captured`.
    Captured,
    /// Lines were added and Autter has no record for any of them (every added
    /// line is Unknown): capture missed them. `--strict` exits 3.
    NothingCaptured,
    /// Nothing was added, so there was nothing to capture.
    NoAdditions,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct CaptureSummary {
    pub status: CaptureStatus,
    /// The commit has an authorship note (for `status`: checkpoints exist
    /// since the last commit).
    pub note_present: bool,
}

/// The fields every stats-shaped `--json` document shares.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct StatsSummary {
    pub lines: LineBreakdown,
    pub percent: PercentBreakdown,
    pub capture: CaptureSummary,
}

impl StatsSummary {
    /// `note_present` decides whether unattributed lines are Untracked (there
    /// is data, it just doesn't cover them) or Unknown (there is no data).
    pub fn from_stats(stats: &CommitStats, note_present: bool) -> Self {
        let unknown = if note_present {
            0
        } else {
            stats.unknown_additions
        };
        Self::with_unknown_lines(stats, unknown, note_present)
    }

    /// Like [`Self::from_stats`] for a view that covers more than one record
    /// (a commit range, the working tree): `unknown_lines` of the lines
    /// without attestation have no record behind them and are Unknown; the
    /// rest are Untracked.
    ///
    /// Capture status: `NothingCaptured` only when lines were added and every
    /// one of them is Unknown. A line Autter recorded as untracked is still a
    /// line it recorded.
    pub fn with_unknown_lines(stats: &CommitStats, unknown_lines: u32, note_present: bool) -> Self {
        let unattributed = stats.unknown_additions;
        let unknown = unknown_lines.min(unattributed);
        let untracked = unattributed - unknown;
        let lines = LineBreakdown {
            added: stats.ai_additions + stats.human_additions + unattributed,
            deleted: stats.git_diff_deleted_lines,
            ai: stats.ai_additions,
            human: stats.human_additions,
            untracked,
            unknown,
        };
        let pct = |n| crate::commands::output::percent(n, lines.added);
        let status = if lines.added == 0 {
            CaptureStatus::NoAdditions
        } else if lines.ai + lines.human + lines.untracked == 0 {
            CaptureStatus::NothingCaptured
        } else {
            CaptureStatus::Captured
        };
        Self {
            lines,
            percent: PercentBreakdown {
                ai: pct(lines.ai),
                human: pct(lines.human),
                untracked: pct(lines.untracked),
                unknown: pct(lines.unknown),
            },
            capture: CaptureSummary {
                status,
                note_present,
            },
        }
    }
}

/// `autter stats <commit> --json`: the commit, the original [`CommitStats`]
/// fields (kept for existing readers) and the [`StatsSummary`].
#[derive(Debug, Clone, Serialize)]
pub struct CommitStatsReport {
    pub commit: String,
    #[serde(flatten)]
    pub stats: CommitStats,
    /// Why lines may lack a record (`autter stats` only; `log` leaves it out).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<RecordStatus>,
    #[serde(flatten)]
    pub summary: StatsSummary,
}

impl CommitStatsReport {
    pub fn new(commit: String, stats: CommitStats, note_present: bool) -> Self {
        let summary = StatsSummary::from_stats(&stats, note_present);
        Self {
            commit,
            stats,
            record: None,
            summary,
        }
    }

    /// The report for `autter stats`: unattributed lines are Untracked when
    /// the commit is recorded and Unknown when it is not, as the terminal
    /// view labels them.
    pub fn with_record(commit: String, stats: CommitStats, record: RecordStatus) -> Self {
        let summary = StatsSummary::from_stats(&stats, record.is_recorded());
        Self {
            commit,
            stats,
            record: Some(record),
            summary,
        }
    }
}

/// Lines printed under `autter stats` for one commit: why lines have no
/// record (with at most one next step) and what the AI share measures.
fn stats_footer(
    stats: &CommitStats,
    record: &RecordStatus,
    unsupported_agent_detected: bool,
) -> String {
    let mut out = String::new();
    // A commit with no note but an agent's author email gets simulated AI
    // attribution; don't call it unrecorded.
    let simulated_agent_commit = !record.is_recorded() && stats.ai_additions > 0;
    if !simulated_agent_commit
        && let Some(explanation) = crate::authorship::record_status::explain_with_agents(
            record,
            stats,
            unsupported_agent_detected,
        )
    {
        out.push_str(&explanation.render());
    }
    if stats.ai_additions > 0 {
        out.push_str(AI_SHARE_MEANING);
        out.push('\n');
    }
    out
}

/// Untracked share at or above which `autter stats` checks for AI tools that
/// autter cannot capture.
const HIGH_UNTRACKED_SHARE: f64 = 0.5;

/// True when `untracked_lines` is at least half of the added lines.
fn untracked_lines_share_is_high(stats: &CommitStats, untracked_lines: u32) -> bool {
    let total = stats.human_additions + stats.unknown_additions + stats.ai_additions;
    total > 0 && f64::from(untracked_lines) / f64::from(total) >= HIGH_UNTRACKED_SHARE
}

/// Whether a view in which `untracked_lines` of the added lines are untracked
/// (recorded, with no agent behind them) should look for AI tools Autter
/// cannot capture: the untracked share is high.
pub fn untracked_share_warrants_agent_check(stats: &CommitStats, untracked_lines: u32) -> bool {
    untracked_lines > 0 && untracked_lines_share_is_high(stats, untracked_lines)
}

/// The unsupported-agent notice for such a view: `None` unless the untracked
/// share is high and an AI tool Autter cannot capture is installed. Used by
/// `stats` for one commit and for a range; `log` shares the two halves.
pub fn unsupported_agent_notice_for(stats: &CommitStats, untracked_lines: u32) -> Option<String> {
    if !untracked_share_warrants_agent_check(stats, untracked_lines) {
        return None;
    }
    let agents = crate::mdm::unsupported_agents::detect_unsupported_agents();
    unsupported_agent_notice(&agents)
}

/// One-line notice naming detected agents whose edits autter cannot capture.
pub fn unsupported_agent_notice(
    agents: &[crate::mdm::unsupported_agents::UnsupportedAgent],
) -> Option<String> {
    let first = agents.first()?;
    let names = agents.iter().map(|a| a.name).collect::<Vec<_>>().join(", ");
    Some(if agents.len() == 1 {
        format!(
            "Note: {}. If it wrote this code, that is why it shows as untracked.\n",
            crate::mdm::unsupported_agents::not_captured_message(first)
        )
    } else {
        format!(
            "Note: {names} detected — autter cannot capture their edits yet; they will be attributed to you. If one of them wrote this code, that is why it shows as untracked.\n"
        )
    })
}

/// Print the stats bar and counts for a commit Autter recorded. Returns the
/// text; prints it only when `is_interactive` is set.
pub fn write_stats_to_terminal(stats: &CommitStats, is_interactive: bool) -> String {
    write_stats_to_terminal_with(stats, is_interactive, LineCategory::Untracked)
}

/// Like [`write_stats_to_terminal`], naming lines without attestation
/// `unattributed` ([`LineCategory::Untracked`] when the commit has a record,
/// [`LineCategory::Unknown`] when it has none).
pub fn write_stats_to_terminal_with(
    stats: &CommitStats,
    is_interactive: bool,
    unattributed: LineCategory,
) -> String {
    write_stats_to_terminal_split(
        stats,
        is_interactive,
        unknown_lines_for(stats, unattributed),
    )
}

/// Like [`write_stats_to_terminal_with`] for a view that mixes recorded and
/// unrecorded commits: `unknown_lines` of the lines without attestation are
/// shown as unknown, the rest as untracked.
pub fn write_stats_to_terminal_split(
    stats: &CommitStats,
    is_interactive: bool,
    unknown_lines: u32,
) -> String {
    // Honor the project-wide color decision (NO_COLOR / --no-color / non-TTY).
    let output = render_stats_output_split(
        stats,
        crate::commands::arg_parser::use_color(),
        unknown_lines,
    );
    if is_interactive {
        print!("{output}");
    }
    output
}

/// How many of the lines without attestation are Unknown when they all carry
/// the one label `unattributed`.
fn unknown_lines_for(stats: &CommitStats, unattributed: LineCategory) -> u32 {
    if unattributed == LineCategory::Unknown {
        stats.unknown_additions
    } else {
        0
    }
}

/// Rendering half of [`write_stats_to_terminal`], with the color decision passed
/// in rather than read from the environment. `use_color()` depends on stdout
/// being a TTY, which is never true under `cargo test`, so snapshots have to
/// drive this directly to stay stable across machines.
///
/// Two lines: a bar running from human (`█`) through untracked or unknown
/// (`·`) to AI (`░`), then each category's line count and share.
fn render_stats_output(stats: &CommitStats, colorize: bool, unattributed: LineCategory) -> String {
    render_stats_output_split(stats, colorize, unknown_lines_for(stats, unattributed))
}

/// [`render_stats_output`] with the lines without attestation divided into
/// `unknown_lines` unknown and the rest untracked.
fn render_stats_output_split(stats: &CommitStats, colorize: bool, unknown_lines: u32) -> String {
    let mut output = String::new();

    // Set maximum bar width to 40 characters
    let bar_width: usize = 40;
    let left = LineCategory::Human.label();
    let right = LineCategory::Ai.label();
    let indent = " ".repeat(left.len() + 1);

    // Handle deletion-only commits (no additions)
    if stats.git_diff_added_lines == 0 && stats.git_diff_deleted_lines > 0 {
        output.push_str(left);
        output.push(' ');
        if colorize {
            output.push_str("\x1b[90m"); // Gray color
        }
        output.push_str(&" ".repeat(bar_width)); // Gray bar
        if colorize {
            output.push_str("\x1b[0m"); // Reset color
        }
        output.push(' ');
        output.push_str(right);
        output.push('\n');
        output.push_str(&format!(
            "{indent}no lines added \u{00B7} {} deleted\n",
            stats.git_diff_deleted_lines
        ));
        return output;
    }

    // Calculate total additions: known human + untracked/unknown + AI
    let total_additions = stats.human_additions + stats.unknown_additions + stats.ai_additions;

    // Determine whether to show the untracked segment (raw float check, before rounding)
    let untracked_pct_raw = if total_additions > 0 {
        stats.unknown_additions as f64 / total_additions as f64 * 100.0
    } else {
        0.0
    };
    let show_untracked = untracked_pct_raw > 1.0;

    // Calculate human bar segment
    let human_bars = if total_additions > 0 {
        ((stats.human_additions as f64 / total_additions as f64) * bar_width as f64) as usize
    } else {
        0
    };

    // Ensure human contributions get at least 2 visible blocks if they have more than 1 line
    let min_human_bars = if stats.human_additions > 1 { 2 } else { 0 };
    let final_human_bars = human_bars.max(min_human_bars);

    // Distribute remaining width between untracked and AI proportionally.
    // When untracked is below the 1% threshold, all remaining width goes to AI.
    let remaining_width = bar_width.saturating_sub(final_human_bars);
    let (final_untracked_bars, final_ai_bars) = if show_untracked {
        let total_other = stats.unknown_additions + stats.ai_additions;
        let untracked_bars = if total_other > 0 {
            ((stats.unknown_additions as f64 / total_other as f64) * remaining_width as f64)
                as usize
        } else {
            0
        };
        (
            untracked_bars,
            remaining_width.saturating_sub(untracked_bars),
        )
    } else {
        (0, remaining_width)
    };

    output.push_str(left);
    output.push(' ');
    output.push_str(&"█".repeat(final_human_bars)); // known human (attested)
    output.push_str(&"·".repeat(final_untracked_bars)); // untracked / unknown
    output.push_str(&"░".repeat(final_ai_bars)); // AI
    output.push(' ');
    output.push_str(right);
    output.push('\n');

    output.push_str(&indent);
    output.push_str(&counts_line(stats, unknown_lines, colorize));
    output.push('\n');

    output
}

/// `12 human (30%) · 8 untracked (20%) · 20 AI (50%) · 40 added, 3 deleted`.
/// Categories with no lines are left out, except AI, which always shows.
/// `unknown_lines` of the lines without attestation are unknown, the rest
/// untracked.
fn counts_line(stats: &CommitStats, unknown_lines: u32, colorize: bool) -> String {
    let unknown_lines = unknown_lines.min(stats.unknown_additions);
    let untracked_lines = stats.unknown_additions - unknown_lines;
    let total = stats.human_additions + stats.unknown_additions + stats.ai_additions;
    let percent = |count: u32| -> u32 {
        if total == 0 {
            0
        } else {
            ((f64::from(count) / f64::from(total)) * 100.0).round() as u32
        }
    };
    let mut parts = Vec::new();
    let mut push = |category: LineCategory, count: u32, always: bool| {
        if count == 0 && !always {
            return;
        }
        let label = if category == LineCategory::Untracked && colorize && use_hyperlinks() {
            // OSC 8 link on terminals that render it; color alone isn't enough
            // (Windows ConHost with VT enabled prints OSC 8 literally).
            "\x1b]8;;https://autter.dev/docs/cli/untracked\x1b\\\x1b[4muntracked\x1b[24m\x1b]8;;\x1b\\"
                .to_string()
        } else {
            category.label().to_string()
        };
        parts.push(format!("{count} {label} ({}%)", percent(count)));
    };
    push(LineCategory::Human, stats.human_additions, false);
    push(LineCategory::Untracked, untracked_lines, false);
    push(LineCategory::Unknown, unknown_lines, false);
    push(LineCategory::Ai, stats.ai_additions, true);

    let mut line = parts.join(" \u{00B7} ");
    line.push_str(&format!(
        " \u{00B7} {} added, {} deleted",
        stats.git_diff_added_lines, stats.git_diff_deleted_lines
    ));
    line
}

fn use_hyperlinks() -> bool {
    std::io::stdout().is_terminal() && crate::commands::arg_parser::use_hyperlinks()
}

#[allow(dead_code)]
pub fn write_stats_to_markdown(stats: &CommitStats) -> String {
    let mut output = String::new();

    // Set maximum bar width to 20 characters
    let bar_width: usize = 20;

    // Handle deletion-only commits (no additions)
    if stats.git_diff_added_lines == 0 && stats.git_diff_deleted_lines > 0 {
        output.push_str("(no additions)");
        output.push('\n');
        return output;
    }

    // Calculate total additions for the progress bar
    let total_additions = stats.git_diff_added_lines;

    // Human additions: known-human attested + unattested
    let pure_human = stats.human_additions + stats.unknown_additions;
    // AI = AI lines accepted
    let pure_ai = stats.ai_accepted;

    // Calculate percentages for display
    let pure_human_percentage = if total_additions > 0 {
        ((pure_human as f64 / total_additions as f64) * 100.0).round() as u32
    } else {
        0
    };
    let ai_percentage = if total_additions > 0 {
        ((pure_ai as f64 / total_additions as f64) * 100.0).round() as u32
    } else {
        0
    };

    // Calculate bar sizes
    let pure_human_bars = if total_additions > 0 {
        let calculated =
            ((pure_human as f64 / total_additions as f64) * bar_width as f64).round() as usize;
        // Ensure at least 1 block if value > 0
        if pure_human > 0 && calculated == 0 {
            1
        } else {
            calculated
        }
    } else {
        0
    };

    let ai_bars = if total_additions > 0 {
        let calculated =
            ((pure_ai as f64 / total_additions as f64) * bar_width as f64).round() as usize;
        // Ensure at least 1 block if value > 0
        if pure_ai > 0 && calculated == 0 {
            1
        } else {
            calculated
        }
    } else {
        0
    };

    output.push_str("Stats powered by [Autter](https://github.com/autter-dev/autter-cli)\n\n");
    // Build the fenced code block
    output.push_str("```text\n");

    // Human line: dark blocks for human, light blocks for rest
    output.push_str("🧠 you    ");
    output.push_str(&"█".repeat(pure_human_bars));
    output.push_str(&"░".repeat(bar_width.saturating_sub(pure_human_bars)));
    output.push_str(&format!("  {}%\n", pure_human_percentage));

    // AI line: light blocks for non-ai, dark blocks for ai
    output.push_str("🤖 ai     ");
    output.push_str(&"░".repeat(bar_width.saturating_sub(ai_bars)));
    output.push_str(&"█".repeat(ai_bars));
    output.push_str(&format!("  {}%\n", ai_percentage));

    output.push_str("```");

    // Add details section
    output.push_str("\n\n<details>\n");
    output.push_str("<summary>More stats</summary>\n\n");

    // Find top model by accepted lines
    if !stats.tool_model_breakdown.is_empty()
        && let Some((model_name, model_stats)) = stats
            .tool_model_breakdown
            .iter()
            .max_by_key(|(_, stats)| stats.ai_accepted)
    {
        output.push_str(&format!(
            "- Top model: {} ({} accepted lines)\n",
            model_name, model_stats.ai_accepted
        ));
    }

    output.push_str("\n</details>");

    output
}

/// Calculate commit stats from an authorship log
/// This helper can work with both fetched and in-memory authorship logs
pub fn stats_from_authorship_log(
    _authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
    git_diff_added_lines: u32,
    git_diff_deleted_lines: u32,
    ai_accepted: u32,
    known_human_accepted: u32,
    ai_accepted_by_tool: &BTreeMap<String, u32>,
) -> CommitStats {
    let mut commit_stats = CommitStats {
        human_additions: 0,
        unknown_additions: 0,
        ai_additions: 0,
        ai_accepted,
        tool_model_breakdown: BTreeMap::new(),
        git_diff_deleted_lines,
        git_diff_added_lines,
    };

    // Update tool-level accepted counts using diff-based attribution.
    for (tool_model, accepted) in ai_accepted_by_tool {
        let tool_stats = commit_stats
            .tool_model_breakdown
            .entry(tool_model.clone())
            .or_default();
        tool_stats.ai_accepted = *accepted;
    }

    // AI additions = ai_accepted (no mixed component)
    commit_stats.ai_additions = commit_stats.ai_accepted;

    // Set ai_additions for each tool: ai_additions = ai_accepted
    for tool_stats in commit_stats.tool_model_breakdown.values_mut() {
        tool_stats.ai_additions = tool_stats.ai_accepted;
    }

    // KnownHuman-attested additions (positively identified as human-authored)
    commit_stats.human_additions = known_human_accepted;

    // Unknown additions: lines with no attestation at all (not AI-accepted, not KnownHuman)
    commit_stats.unknown_additions = git_diff_added_lines
        .saturating_sub(commit_stats.ai_accepted)
        .saturating_sub(known_human_accepted);

    commit_stats
}

pub fn stats_for_commit_stats(
    repo: &Repository,
    commit_sha: &str,
    ignore_patterns: &[String],
) -> Result<CommitStats, AutterError> {
    let authorship_log = get_authorship(repo, commit_sha);
    stats_for_commit_stats_with_authorship(
        repo,
        commit_sha,
        ignore_patterns,
        authorship_log.as_ref(),
    )
}

pub fn stats_for_commit_stats_with_authorship(
    repo: &Repository,
    commit_sha: &str,
    ignore_patterns: &[String],
    authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
) -> Result<CommitStats, AutterError> {
    let commit_obj = repo.revparse_single(commit_sha)?.peel_to_commit()?;
    let parent_count = commit_obj.parent_count()?;

    if parent_count > 1 {
        let mut parents = Vec::with_capacity(parent_count);
        for index in 0..parent_count {
            parents.push(commit_obj.parent(index)?.id());
        }
        return stats_for_merge_commit(repo, commit_sha, &parents, ignore_patterns, authorship_log);
    }

    let parent_sha = if parent_count == 0 {
        None
    } else {
        Some(commit_obj.parent(0)?.id())
    };

    stats_for_commit_stats_with_parent_and_authorship(
        repo,
        commit_sha,
        parent_sha.as_deref(),
        ignore_patterns,
        authorship_log,
    )
}

pub fn stats_for_commit_stats_with_parent_and_authorship(
    repo: &Repository,
    commit_sha: &str,
    parent_sha: Option<&str>,
    ignore_patterns: &[String],
    authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
) -> Result<CommitStats, AutterError> {
    use crate::commands::diff::get_diff_with_line_numbers;

    let from_ref = parent_sha.unwrap_or("4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    let hunks = get_diff_with_line_numbers(repo, from_ref, commit_sha)?;
    stats_for_commit_stats_from_hunks(repo, commit_sha, ignore_patterns, &hunks, authorship_log)
}

/// Stats for a merge commit: only the lines the merge itself introduced, that
/// is, the lines of its tree that are in none of its parents (a conflict
/// resolution, an edit made while merging). What the merged branches brought
/// in belongs to their own commits and is not counted again here.
///
/// A merge with no lines of its own reports no additions, so `--strict` has
/// nothing to judge and passes. One with such lines is judged on them like any
/// other commit. Deleted lines are not counted for a merge.
fn stats_for_merge_commit(
    repo: &Repository,
    commit_sha: &str,
    parents: &[String],
    ignore_patterns: &[String],
    authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
) -> Result<CommitStats, AutterError> {
    let added_lines_by_file = merge_commit_own_lines(repo, commit_sha, parents, ignore_patterns)?;
    let git_diff_added_lines = added_lines_by_file
        .values()
        .map(|lines| lines.len() as u32)
        .sum();
    let (ai_accepted, known_human_accepted, ai_accepted_by_tool) =
        accepted_lines_from_attestations(authorship_log, &added_lines_by_file, false);

    Ok(stats_from_authorship_log(
        authorship_log,
        git_diff_added_lines,
        0,
        ai_accepted,
        known_human_accepted,
        &ai_accepted_by_tool,
    ))
}

/// Per file, the sorted line numbers of `commit_sha` that are added relative
/// to every one of `parents`: the `++` lines of git's combined diff.
fn merge_commit_own_lines(
    repo: &Repository,
    commit_sha: &str,
    parents: &[String],
    ignore_patterns: &[String],
) -> Result<HashMap<String, Vec<u32>>, AutterError> {
    let ignore_matcher = build_ignore_matcher(ignore_patterns);
    let mut own: Option<HashMap<String, Vec<u32>>> = None;
    for parent in parents {
        let mut added = repo.diff_added_lines(parent, commit_sha, None)?;
        added.retain(|file, lines| {
            lines.sort_unstable();
            lines.dedup();
            !lines.is_empty() && !should_ignore_file_with_matcher(file, &ignore_matcher)
        });
        let remaining = match own.take() {
            None => added,
            Some(previous) => previous
                .into_iter()
                .filter_map(|(file, lines)| {
                    let also_added = added.get(&file)?;
                    let kept: Vec<u32> = lines
                        .into_iter()
                        .filter(|line| also_added.binary_search(line).is_ok())
                        .collect();
                    (!kept.is_empty()).then_some((file, kept))
                })
                .collect(),
        };
        let done = remaining.is_empty();
        own = Some(remaining);
        if done {
            break;
        }
    }
    Ok(own.unwrap_or_default())
}

#[doc(hidden)]
pub fn accepted_lines_from_attestations(
    authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
    added_lines_by_file: &HashMap<String, Vec<u32>>,
    is_merge_commit: bool,
) -> (u32, u32, BTreeMap<String, u32>) {
    // returns (ai_accepted, known_human_accepted, per_tool_model)
    if is_merge_commit {
        return (0, 0, BTreeMap::new());
    }

    let mut total_ai_accepted = 0u32;
    let mut known_human_accepted = 0u32;
    let mut per_tool_model = BTreeMap::new();

    let Some(log) = authorship_log else {
        return (0, 0, per_tool_model);
    };

    for file_attestation in &log.attestations {
        let Some(added_lines) = added_lines_by_file.get(&file_attestation.file_path) else {
            continue;
        };

        for entry in &file_attestation.entries {
            // KnownHuman entries (h_ prefix): count as known-human-attested lines.
            if entry.hash.starts_with("h_") {
                let accepted = entry
                    .line_ranges
                    .iter()
                    .map(|line_range| line_range_overlap_len(line_range, added_lines))
                    .sum::<u32>();
                if accepted > 0 {
                    known_human_accepted += accepted;
                }
                continue;
            }

            let accepted = entry
                .line_ranges
                .iter()
                .map(|line_range| line_range_overlap_len(line_range, added_lines))
                .sum::<u32>();

            if accepted == 0 {
                continue;
            }

            total_ai_accepted += accepted;

            // Session entries (s_ prefix): look up in sessions map
            if entry.hash.starts_with("s_") {
                let session_key = entry.hash.split("::").next().unwrap_or(&entry.hash);
                if let Some(session_record) = log.metadata.sessions.get(session_key) {
                    let tool_model = format!(
                        "{}::{}",
                        session_record.agent_id.tool, session_record.agent_id.model
                    );
                    *per_tool_model.entry(tool_model).or_insert(0) += accepted;
                }
            } else if let Some(prompt_record) = log.metadata.prompts.get(&entry.hash) {
                let tool_model = format!(
                    "{}::{}",
                    prompt_record.agent_id.tool, prompt_record.agent_id.model
                );
                *per_tool_model.entry(tool_model).or_insert(0) += accepted;
            }
        }
    }

    (total_ai_accepted, known_human_accepted, per_tool_model)
}

#[doc(hidden)]
pub fn line_range_overlap_len(range: &LineRange, added_lines: &[u32]) -> u32 {
    match range {
        LineRange::Single(line) => u32::from(added_lines.binary_search(line).is_ok()),
        LineRange::Range(start, end) => {
            let start_idx = added_lines.partition_point(|line| *line < *start);
            let end_idx = added_lines.partition_point(|line| *line <= *end);
            end_idx.saturating_sub(start_idx) as u32
        }
    }
}

/// Like `stats_for_commit_stats` but accepts pre-computed diff hunks and authorship log,
/// avoiding redundant git subprocess calls in the post-commit hook path.
pub fn stats_for_commit_stats_from_hunks(
    repo: &Repository,
    commit_sha: &str,
    ignore_patterns: &[String],
    hunks: &[crate::commands::diff::DiffHunk],
    authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
) -> Result<CommitStats, AutterError> {
    let commit_obj = repo.revparse_single(commit_sha)?.peel_to_commit()?;
    let parent_count = commit_obj.parent_count()?;
    let is_merge_commit = parent_count > 1;

    let ignore_matcher = build_ignore_matcher(ignore_patterns);

    let mut git_diff_added_lines = 0u32;
    let mut git_diff_deleted_lines = 0u32;
    let mut added_lines_by_file: HashMap<String, Vec<u32>> = HashMap::new();

    for hunk in hunks {
        if should_ignore_file_with_matcher(&hunk.file_path, &ignore_matcher) {
            continue;
        }
        git_diff_added_lines += hunk.added_lines.len() as u32;
        git_diff_deleted_lines += hunk.deleted_lines.len() as u32;

        if !is_merge_commit && !hunk.added_lines.is_empty() {
            added_lines_by_file
                .entry(hunk.file_path.clone())
                .or_default()
                .extend(hunk.added_lines.iter().copied());
        }
    }

    for lines in added_lines_by_file.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }

    // A commit with no authorship note whose author email matches a known AI
    // agent (e.g. Codex cloud, Claude web) gets all added lines attributed to
    // that agent — the same fallback `autter blame` applies per hunk.
    let simulated_log;
    let authorship_log = if authorship_log.is_none()
        && !is_merge_commit
        && !added_lines_by_file.is_empty()
        && let Some(tool) = commit_obj
            .author_email()
            .ok()
            .as_deref()
            .and_then(crate::authorship::agent_detection::match_email_to_agent)
    {
        simulated_log =
            crate::authorship::agent_detection::simulate_agent_authorship_for_added_lines(
                commit_sha,
                tool,
                &added_lines_by_file,
            );
        Some(&simulated_log)
    } else {
        authorship_log
    };

    let (ai_accepted, known_human_accepted, ai_accepted_by_tool) =
        accepted_lines_from_attestations(authorship_log, &added_lines_by_file, is_merge_commit);

    Ok(stats_from_authorship_log(
        authorship_log,
        git_diff_added_lines,
        git_diff_deleted_lines,
        ai_accepted,
        known_human_accepted,
        &ai_accepted_by_tool,
    ))
}

/// Get git diff statistics between commit and its parent
/// Uses the same diff engine as autter diff to properly handle renames
pub fn get_git_diff_stats(
    repo: &Repository,
    commit_sha: &str,
    ignore_patterns: &[String],
) -> Result<(u32, u32), AutterError> {
    use crate::commands::diff::get_diff_with_line_numbers;

    let commit_obj = repo.revparse_single(commit_sha)?.peel_to_commit()?;
    let parent_count = commit_obj.parent_count()?;

    // For merge commits, return (0, 0) to match the behavior of `git show --numstat`
    // which shows a combined diff (typically 0 lines for clean merges)
    if parent_count > 1 {
        return Ok((0, 0));
    }

    let from_ref = if parent_count == 0 {
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
    } else {
        commit_obj.parent(0)?.id()
    };

    // Use the diff engine which properly handles renames with --find-renames=1%
    let hunks = get_diff_with_line_numbers(repo, &from_ref, commit_sha)?;

    let ignore_matcher = build_ignore_matcher(ignore_patterns);
    let mut added_lines = 0u32;
    let mut deleted_lines = 0u32;

    for hunk in hunks {
        if should_ignore_file_with_matcher(&hunk.file_path, &ignore_matcher) {
            continue;
        }
        added_lines += hunk.added_lines.len() as u32;
        deleted_lines += hunk.deleted_lines.len() as u32;
    }

    Ok((added_lines, deleted_lines))
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_debug_snapshot;

    fn stats_with(human: u32, unknown: u32, ai: u32) -> CommitStats {
        CommitStats {
            human_additions: human,
            unknown_additions: unknown,
            ai_additions: ai,
            git_diff_added_lines: human + unknown + ai,
            ..Default::default()
        }
    }

    #[test]
    fn untracked_share_threshold() {
        assert!(untracked_lines_share_is_high(&stats_with(0, 10, 0), 10));
        assert!(untracked_lines_share_is_high(&stats_with(5, 5, 0), 5));
        assert!(!untracked_lines_share_is_high(&stats_with(6, 4, 0), 4));
        assert!(!untracked_lines_share_is_high(&stats_with(0, 0, 0), 0));
        // In a range, lines from commits with no record are unknown, not
        // untracked: they don't count towards the untracked share.
        assert!(!untracked_lines_share_is_high(&stats_with(0, 10, 0), 4));
    }

    #[test]
    fn unsupported_agent_notice_names_the_agent() {
        use crate::mdm::unsupported_agents::UnsupportedAgent;
        assert_eq!(unsupported_agent_notice(&[]), None);
        let trae = UnsupportedAgent {
            id: "trae",
            name: "Trae",
            evidence: "app".to_string(),
        };
        let notice = unsupported_agent_notice(std::slice::from_ref(&trae)).unwrap();
        assert!(
            notice.contains(
                "Trae detected — its edits are not captured; they will be attributed to you"
            ),
            "{notice}"
        );
        let kiro = UnsupportedAgent {
            id: "kiro",
            name: "Kiro",
            evidence: "app".to_string(),
        };
        let notice = unsupported_agent_notice(&[trae, kiro]).unwrap();
        assert!(notice.contains("Trae, Kiro detected"), "{notice}");
    }

    #[test]
    fn test_terminal_stats_display() {
        // Test with mixed human/AI stats
        let stats = CommitStats {
            human_additions: 50,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 25,
            git_diff_deleted_lines: 15,
            git_diff_added_lines: 80,
            tool_model_breakdown: BTreeMap::new(),
        };

        let mixed_output = render_stats_output(&stats, true, LineCategory::Untracked);
        assert_debug_snapshot!(mixed_output);

        // Test with AI-only stats
        let ai_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 95,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 100,
            tool_model_breakdown: BTreeMap::new(),
        };

        let ai_only_output = render_stats_output(&ai_stats, true, LineCategory::Untracked);
        assert_debug_snapshot!(ai_only_output);

        // Test with human-only stats
        let human_stats = CommitStats {
            human_additions: 75,
            unknown_additions: 0,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 10,
            git_diff_added_lines: 75,
            tool_model_breakdown: BTreeMap::new(),
        };

        let human_only_output = render_stats_output(&human_stats, true, LineCategory::Untracked);
        assert_debug_snapshot!(human_only_output);

        // Test with minimal human contribution (should get at least 2 blocks)
        let minimal_human_stats = CommitStats {
            human_additions: 2,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 95,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 102,
            tool_model_breakdown: BTreeMap::new(),
        };

        let minimal_human_output =
            render_stats_output(&minimal_human_stats, true, LineCategory::Untracked);
        assert_debug_snapshot!(minimal_human_output);

        // Test with deletion-only commit (no additions)
        let deletion_only_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 0,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 25,
            git_diff_added_lines: 0,
            tool_model_breakdown: BTreeMap::new(),
        };

        let deletion_only_output =
            render_stats_output(&deletion_only_stats, true, LineCategory::Untracked);
        assert_debug_snapshot!(deletion_only_output);

        // --- New test cases for untracked segment ---

        // 18% human / 22% untracked / 60% AI — matches the design example
        let untracked_stats = CommitStats {
            human_additions: 180,
            unknown_additions: 220,
            ai_additions: 600,
            ai_accepted: 462,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 1000,
            tool_model_breakdown: BTreeMap::new(),
        };
        let with_untracked_output =
            render_stats_output(&untracked_stats, true, LineCategory::Untracked);
        assert_debug_snapshot!(with_untracked_output);

        // untracked exactly at the 1% threshold — should NOT show untracked segment
        let threshold_stats = CommitStats {
            human_additions: 49,
            unknown_additions: 1,
            ai_additions: 50,
            ai_accepted: 50,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 100,
            tool_model_breakdown: BTreeMap::new(),
        };
        let untracked_at_threshold_output =
            render_stats_output(&threshold_stats, true, LineCategory::Untracked);
        assert_debug_snapshot!(untracked_at_threshold_output);

        // untracked just above 1% threshold (~2%) — should show untracked segment
        let above_threshold_stats = CommitStats {
            human_additions: 97,
            unknown_additions: 2,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 99,
            tool_model_breakdown: BTreeMap::new(),
        };
        let untracked_just_above_output =
            render_stats_output(&above_threshold_stats, true, LineCategory::Untracked);
        assert_debug_snapshot!(untracked_just_above_output);

        // 100% untracked — entire bar is · chars
        let all_untracked_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 100,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 100,
            tool_model_breakdown: BTreeMap::new(),
        };
        let all_untracked_output =
            render_stats_output(&all_untracked_stats, true, LineCategory::Untracked);
        assert_debug_snapshot!(all_untracked_output);

        // A commit with no record names the same lines "unknown".
        let unknown_output = render_stats_output(&all_untracked_stats, true, LineCategory::Unknown);
        assert_debug_snapshot!(unknown_output);

        // OSC 8 hyperlinks need a terminal; under `cargo test` stdout is not one.
        assert!(
            !render_stats_output(&untracked_stats, true, LineCategory::Untracked)
                .contains("\x1b]8;;"),
            "OSC 8 must not leak when stdout is not a terminal"
        );
    }

    #[test]
    fn stats_footer_explains_without_troubleshooting() {
        use crate::authorship::record_status::{MissingCause, RecordStatus};
        let human_only = stats_with(0, 5, 0);
        let footer = stats_footer(&human_only, &RecordStatus::Recorded, false);
        assert_eq!(
            footer,
            "No AI activity detected: Autter recorded this commit, and no agent wrote its lines.\n"
        );
        // With an AI tool Autter cannot capture installed, the footer sits
        // above "If it wrote this code…" and must not rule that out.
        let footer = stats_footer(&human_only, &RecordStatus::Recorded, true);
        assert_eq!(
            footer,
            "No AI activity detected: Autter recorded this commit, and none of the agents it captures wrote its lines.\n"
        );
        assert!(!footer.contains("no agent wrote"), "{footer}");

        let with_ai = stats_with(1, 2, 3);
        assert_eq!(
            stats_footer(&with_ai, &RecordStatus::Recorded, false),
            format!("{AI_SHARE_MEANING}\n")
        );

        let missing = RecordStatus::Missing {
            cause: MissingCause::NotCaptured,
        };
        let footer = stats_footer(&human_only, &missing, false);
        assert_eq!(footer.matches("Next:").count(), 1, "{footer}");
        assert!(!footer.contains("install-hooks"), "{footer}");

        // Simulated agent-email attribution: don't call it unrecorded.
        assert_eq!(
            stats_footer(&stats_with(0, 0, 4), &missing, false),
            format!("{AI_SHARE_MEANING}\n")
        );
    }

    #[test]
    fn capture_status_is_nothing_captured_only_without_a_record() {
        let status = |stats: &CommitStats, note_present: bool| {
            StatsSummary::from_stats(stats, note_present).capture.status
        };
        // Typed by hand, and Autter recorded the commit: capture worked.
        assert_eq!(status(&stats_with(0, 5, 0), true), CaptureStatus::Captured);
        // The same lines with no record: capture missed the commit.
        assert_eq!(
            status(&stats_with(0, 5, 0), false),
            CaptureStatus::NothingCaptured
        );
        // AI or known-human lines are captured either way.
        assert_eq!(status(&stats_with(0, 2, 3), true), CaptureStatus::Captured);
        assert_eq!(status(&stats_with(0, 0, 3), false), CaptureStatus::Captured);
        assert_eq!(status(&stats_with(1, 0, 0), true), CaptureStatus::Captured);
        assert_eq!(
            status(&stats_with(0, 0, 0), false),
            CaptureStatus::NoAdditions
        );
    }

    #[test]
    fn a_range_splits_unattested_lines_into_untracked_and_unknown() {
        // 6 lines without attestation, 4 of them from commits with no record.
        let stats = stats_with(1, 6, 3);
        let summary = StatsSummary::with_unknown_lines(&stats, 4, true);
        assert_eq!(summary.lines.untracked, 2);
        assert_eq!(summary.lines.unknown, 4);
        assert_eq!(
            summary.lines.ai
                + summary.lines.human
                + summary.lines.untracked
                + summary.lines.unknown,
            summary.lines.added
        );
        assert_eq!(summary.capture.status, CaptureStatus::Captured);

        // Every added line comes from a commit with no record.
        let all_unknown = StatsSummary::with_unknown_lines(&stats_with(0, 6, 0), 6, true);
        assert_eq!(all_unknown.lines.unknown, 6);
        assert_eq!(all_unknown.capture.status, CaptureStatus::NothingCaptured);

        let text = render_stats_output_split(&stats, false, 4);
        assert!(
            text.contains("1 human (10%) \u{00B7} 2 untracked (20%) \u{00B7} 4 unknown (40%) \u{00B7} 3 AI (30%)"),
            "{text}"
        );
    }

    #[test]
    fn stats_json_adds_record_without_renaming_fields() {
        let stats = stats_with(1, 2, 3);
        let json = serde_json::to_value(CommitStatsReport::with_record(
            "abc".to_string(),
            stats,
            RecordStatus::Recorded,
        ))
        .unwrap();
        for field in [
            "human_additions",
            "unknown_additions",
            "ai_additions",
            "ai_accepted",
            "git_diff_deleted_lines",
            "git_diff_added_lines",
            "tool_model_breakdown",
        ] {
            assert!(json.get(field).is_some(), "missing {field}: {json}");
        }
        assert_eq!(json["record"], serde_json::json!({"status": "recorded"}));
    }

    #[test]
    fn test_markdown_stats_display() {
        // Test with mixed human/AI stats
        let stats = CommitStats {
            human_additions: 50,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 25,
            git_diff_deleted_lines: 15,
            git_diff_added_lines: 80,
            tool_model_breakdown: BTreeMap::new(),
        };

        let mixed_output = write_stats_to_markdown(&stats);
        assert_debug_snapshot!(mixed_output);

        // Test with AI-only stats
        let ai_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 95,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 100,
            tool_model_breakdown: BTreeMap::new(),
        };

        let ai_only_output = write_stats_to_markdown(&ai_stats);
        assert_debug_snapshot!(ai_only_output);

        // Test with human-only stats
        let human_stats = CommitStats {
            human_additions: 75,
            unknown_additions: 0,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 10,
            git_diff_added_lines: 75,
            tool_model_breakdown: BTreeMap::new(),
        };

        let human_only_output = write_stats_to_markdown(&human_stats);
        assert_debug_snapshot!(human_only_output);

        // Test with minimal human contribution (should get at least 2 blocks)
        let minimal_human_stats = CommitStats {
            human_additions: 2,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 95,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 102,
            tool_model_breakdown: BTreeMap::new(),
        };

        let minimal_human_output = write_stats_to_markdown(&minimal_human_stats);
        assert_debug_snapshot!(minimal_human_output);

        // Test with deletion-only commit (no additions)
        let deletion_only_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 0,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 25,
            git_diff_added_lines: 0,
            tool_model_breakdown: BTreeMap::new(),
        };

        let deletion_only_output = write_stats_to_markdown(&deletion_only_stats);
        assert_debug_snapshot!(deletion_only_output);
    }
}
