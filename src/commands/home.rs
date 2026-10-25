//! What bare `autter` shows at a terminal: the brand header with this
//! machine's mode and account, the latest commit's AI share, and the
//! commands worth trying. It reads only local state (config, stored
//! credentials, the local notes cache and one `git diff`), so it stays quick;
//! a row that can't be computed is left out. `autter help` lists everything.

use crate::auth::{AuthState, collect_auth_status};
use crate::authorship::stats::{CommitStats, stats_for_commit_stats};
use crate::config;
use crate::ui::{self, TermCaps};

/// Width of the compact authorship bar.
const BAR_WIDTH: usize = 20;

pub fn print_home() {
    eprint!("{}", render(&gather(), &ui::caps()));
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    NotSetUp,
    Local,
    Connected { account: String },
    SignedOut,
}

#[derive(Debug, Clone)]
struct LastCommit {
    /// `abc1234: subject`.
    label: String,
    stats: CommitStats,
}

struct HomeView {
    version: &'static str,
    mode: Mode,
    cwd: String,
    /// `None` outside a git repository (or with no commits yet).
    last_commit: Option<LastCommit>,
    pending_uploads: Option<i64>,
}

fn gather() -> HomeView {
    let file_config = config::load_file_config_public().unwrap_or_default();
    let mode = if file_config.onboarding_completed != Some(true) {
        Mode::NotSetUp
    } else if !config::Config::fresh().platform_sync_enabled() {
        Mode::Local
    } else {
        let auth = collect_auth_status();
        if matches!(auth.state, AuthState::LoggedIn) {
            let who = auth
                .email
                .clone()
                .or(auth.name.clone())
                .unwrap_or_else(|| "your account".to_string());
            let org = auth
                .orgs
                .iter()
                .find_map(|org| org.org_slug.clone().or(org.org_name.clone()));
            Mode::Connected {
                account: match org {
                    Some(org) => format!("{who} \u{00B7} {org}"),
                    None => who,
                },
            }
        } else {
            Mode::SignedOut
        }
    };
    let pending_uploads = matches!(mode, Mode::Connected { .. })
        .then(|| crate::auth::notice::pending_sync_counts().total());

    HomeView {
        version: env!("CARGO_PKG_VERSION"),
        mode,
        cwd: ui::header::display_cwd(),
        last_commit: last_commit(),
        pending_uploads,
    }
}

fn last_commit() -> Option<LastCommit> {
    let repo = crate::git::repository::find_repository_in_path(".").ok()?;
    let sha = repo.head().ok()?.target().ok()?;
    let patterns = crate::authorship::ignore::effective_ignore_patterns(&repo, &[], &[]);
    let stats = stats_for_commit_stats(&repo, &sha, &patterns).ok()?;
    let label = crate::commands::usage_hints::head_commit_hint(&repo)
        .unwrap_or_else(|| sha.chars().take(7).collect());
    Some(LastCommit { label, stats })
}

fn render(view: &HomeView, caps: &TermCaps) -> String {
    let mode = match &view.mode {
        Mode::NotSetUp => format!(
            "Not set up yet \u{00B7} run {}",
            caps.paint("36", "autter onboard")
        ),
        Mode::Local => "Local mode \u{00B7} nothing leaves this machine".to_string(),
        Mode::Connected { account } => format!("Connected \u{00B7} {account}"),
        Mode::SignedOut => format!(
            "Connected mode, signed out \u{00B7} run {}",
            caps.paint("36", "autter login")
        ),
    };
    let mut out = String::from("\n");
    out.push_str(
        &ui::Header::new("Autter", format!("v{}", view.version), mode, &view.cwd).render(caps),
    );
    out.push('\n');

    let label = |text: &str| caps.paint("2", &format!("{text:<13}"));
    match &view.last_commit {
        Some(commit) => {
            let (sha, subject) = commit
                .label
                .split_once(": ")
                .unwrap_or((commit.label.as_str(), ""));
            out.push_str(&format!(
                "  {}{}  {}\n",
                label("Last commit"),
                caps.paint("36", sha),
                share_summary(&commit.stats, caps)
            ));
            if !subject.is_empty() {
                out.push_str(&format!("  {:13}{}\n", "", caps.paint("2", subject)));
            }
        }
        None => out.push_str(&format!(
            "  {}\n",
            caps.paint(
                "2",
                "Run autter inside a git repository to see its latest commit's authorship."
            )
        )),
    }
    if let Some(pending) = view.pending_uploads {
        let value = if pending == 0 {
            "none pending".to_string()
        } else {
            format!("{pending} pending")
        };
        out.push_str(&format!("  {}{value}\n", label("Uploads")));
    }

    let commands = if view.mode == Mode::NotSetUp {
        "autter onboard \u{00B7} autter help".to_string()
    } else {
        let mut commands = vec!["autter stats", "autter blame <file>"];
        if matches!(view.mode, Mode::Connected { .. }) {
            commands.push("autter dash");
        }
        commands.push("autter help");
        commands.join(" \u{00B7} ")
    };
    out.push('\n');
    out.push_str(&format!("  {}\n\n", caps.paint("2", &commands)));
    out
}

/// `████░░░░  62% AI · claude`, or why there's no share to show.
fn share_summary(stats: &CommitStats, caps: &TermCaps) -> String {
    let added = stats.git_diff_added_lines;
    if added == 0 {
        return caps.paint("2", "no added lines");
    }
    let ai = stats.ai_additions.min(added);
    let human = stats.human_additions.min(added - ai);
    let untracked = added - ai - human;
    if untracked == added {
        return caps.paint("2", "not tracked \u{00B7} run autter doctor");
    }

    let [human_cols, untracked_cols, ai_cols] = bar_columns(human, untracked, ai, BAR_WIDTH);
    let bar = format!(
        "{}{}{}",
        "\u{2588}".repeat(human_cols),
        caps.paint("2", &"\u{00B7}".repeat(untracked_cols)),
        caps.accent(&"\u{2591}".repeat(ai_cols))
    );
    let percent = (f64::from(ai) * 100.0 / f64::from(added)).round() as u32;
    let mut summary = format!("{bar}  {percent}% AI");
    if let Some(tool) = top_tool(stats) {
        summary.push_str(&format!(" \u{00B7} {tool}"));
    }
    summary
}

/// Split `width` columns between human, untracked and AI lines, giving every
/// non-empty part at least one column.
pub(crate) fn bar_columns(human: u32, untracked: u32, ai: u32, width: usize) -> [usize; 3] {
    let parts = [human, untracked, ai];
    let total: u32 = parts.iter().sum();
    if total == 0 {
        return [0, 0, 0];
    }
    let mut columns = parts.map(|part| {
        if part == 0 {
            0
        } else {
            ((f64::from(part) * width as f64 / f64::from(total)).round() as usize).max(1)
        }
    });
    // Rounding can overshoot or undershoot; settle the difference on the
    // widest part.
    let widest = (0..3).max_by_key(|&i| columns[i]).unwrap_or(0);
    let sum: usize = columns.iter().sum();
    if sum > width {
        columns[widest] -= sum - width;
    } else {
        columns[widest] += width - sum;
    }
    columns
}

/// The tool behind most of the commit's AI lines.
fn top_tool(stats: &CommitStats) -> Option<String> {
    stats
        .tool_model_breakdown
        .iter()
        .max_by_key(|(_, headline)| headline.ai_additions)
        .filter(|(_, headline)| headline.ai_additions > 0)
        .map(|(key, _)| key.split("::").next().unwrap_or(key).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::stats::ToolModelHeadlineStats;
    use crate::ui::theme::{Charset, ColorLevel};

    fn caps() -> TermCaps {
        TermCaps {
            tty: true,
            color: ColorLevel::None,
            charset: Charset::Unicode,
            width: 100,
        }
    }

    fn stats(human: u32, untracked: u32, ai: u32) -> CommitStats {
        let mut breakdown = std::collections::BTreeMap::new();
        if ai > 0 {
            breakdown.insert(
                "claude::opus".to_string(),
                ToolModelHeadlineStats {
                    ai_additions: ai,
                    ai_accepted: ai,
                },
            );
        }
        CommitStats {
            human_additions: human,
            unknown_additions: untracked,
            ai_additions: ai,
            ai_accepted: ai,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: human + untracked + ai,
            tool_model_breakdown: breakdown,
        }
    }

    fn view(mode: Mode, last_commit: Option<LastCommit>) -> HomeView {
        HomeView {
            version: "2.3.0",
            pending_uploads: matches!(mode, Mode::Connected { .. }).then_some(0),
            mode,
            cwd: "~/code/app".to_string(),
            last_commit,
        }
    }

    #[test]
    fn connected_home() {
        let commit = LastCommit {
            label: "a1b2c3d: Add retry to the uploader".to_string(),
            stats: stats(30, 8, 62),
        };
        insta::assert_snapshot!(render(
            &view(
                Mode::Connected {
                    account: "you@company.com \u{00B7} acme".to_string()
                },
                Some(commit)
            ),
            &caps()
        ));
    }

    #[test]
    fn outside_a_repo_and_not_set_up() {
        let text = render(&view(Mode::NotSetUp, None), &caps());
        assert!(
            text.contains("Not set up yet · run autter onboard"),
            "{text}"
        );
        assert!(text.contains("inside a git repository"), "{text}");
        assert!(text.contains("autter onboard · autter help"), "{text}");
        assert!(!text.contains("Uploads"), "{text}");
    }

    #[test]
    fn share_handles_empty_and_untracked_commits() {
        assert_eq!(share_summary(&stats(0, 0, 0), &caps()), "no added lines");
        assert_eq!(
            share_summary(&stats(0, 12, 0), &caps()),
            "not tracked · run autter doctor"
        );
        assert_eq!(
            share_summary(&stats(10, 0, 0), &caps()),
            format!("{}  0% AI", "\u{2588}".repeat(BAR_WIDTH))
        );
    }

    #[test]
    fn bar_always_fills_its_width_and_shows_small_parts() {
        for parts in [(1, 0, 999), (5, 3, 2), (1, 1, 1), (100, 0, 0)] {
            let columns = bar_columns(parts.0, parts.1, parts.2, BAR_WIDTH);
            assert_eq!(columns.iter().sum::<usize>(), BAR_WIDTH, "{parts:?}");
        }
        assert_eq!(bar_columns(1, 0, 999, BAR_WIDTH)[0], 1);
        assert_eq!(bar_columns(0, 0, 0, BAR_WIDTH), [0, 0, 0]);
    }
}
