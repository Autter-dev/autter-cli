//! One-line setup reminders on interactive `autter` commands.
//!
//! - Setup never finished: npm hides install-script output (and npm 10+ may
//!   not run it at all), so someone who installed with npm never sees "run
//!   `autter onboard`". Said at most once a day until onboarding has run.
//! - A coding agent installed after onboarding: its edits are not captured
//!   until its hooks are installed, and nothing said so before `autter
//!   doctor`. Said once per agent, never for an agent the user turned down
//!   during onboarding, and nothing is installed.
//! - An npm install with a newer release out: the npm command, once per
//!   release (see `upgrade::maybe_print_npm_update_notice`).
//!
//! Only for a person at a terminal: never in CI, from an agent hook, with
//! `--json`, or on the commands where it would be noise (`onboard`, `help`,
//! `--version`, `doctor`, the install commands, ...).

use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use std::path::PathBuf;

const DAY_SECS: u64 = 24 * 60 * 60;

/// The commands a person runs to look at their work. Everything else (setup,
/// diagnostics, hooks, internal plumbing, servers) never carries a notice.
const NOTICE_COMMANDS: &[&str] = &[
    "stats",
    "status",
    "log",
    "show",
    "blame",
    "diff",
    "whoami",
    "sync",
    "wrapped",
    "badge",
    "dash",
    "dashboard",
    "personal-dashboard",
    "ask",
    "logs",
    "threads",
    "memory",
    "context",
    "telemetry",
    "config",
    "show-prompt",
];

pub const ONBOARD_NOTICE: &str = "autter: setup is not finished, so edits are not captured yet. Run `autter onboard` to finish it.";

/// What the gate looks at, gathered once so the rules can be tested.
#[derive(Debug, Clone, Copy)]
pub struct NoticeEnv {
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    pub ci: bool,
    /// Run by an agent (hook, background agent) rather than a person.
    pub agent: bool,
}

impl NoticeEnv {
    fn current() -> Self {
        Self {
            stdout_tty: std::io::stdout().is_terminal(),
            stderr_tty: std::io::stderr().is_terminal(),
            ci: crate::commands::output::ci_mode() || crate::commands::upgrade::running_in_ci(),
            agent: crate::utils::is_in_background_agent()
                || std::env::var_os("AUTTER_CHECKPOINT_HOOK").is_some(),
        }
    }
}

/// The subcommand, past the leading global flags (`-C <path>`, `--json`, ...).
fn subcommand(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-C" | "-c" => {
                iter.next();
            }
            a if a.starts_with('-') => {}
            a => return Some(a),
        }
    }
    None
}

/// Whether this command run may carry a setup notice at all.
pub fn notices_allowed(args: &[String], env: NoticeEnv) -> bool {
    if !env.stdout_tty || !env.stderr_tty || env.ci || env.agent {
        return false;
    }
    let Some(command) = subcommand(args) else {
        // Bare `autter` shows the home screen, which already says
        // "Not set up yet · run autter onboard".
        return false;
    };
    if !NOTICE_COMMANDS.contains(&command) {
        return false;
    }
    !args.iter().any(|a| {
        matches!(a.as_str(), "--json" | "-h" | "--help" | "--hook-input")
            || a.starts_with("--json=")
            || a.starts_with("--hook-input=")
    })
}

/// The onboarding reminder, when onboarding never completed and the last one
/// was at least a day ago.
pub fn onboard_notice_due(onboarding_completed: bool, last_shown: Option<u64>, now: u64) -> bool {
    !onboarding_completed && last_shown.is_none_or(|at| now.saturating_sub(at) >= DAY_SECS)
}

/// Agents the user was told about or turned down, and when installed agents
/// were last looked for.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentNoticeState {
    /// Installer ids left unselected in onboarding's "Install integrations?"
    /// question.
    #[serde(default)]
    pub declined: Vec<String>,
    /// Installer ids already named in a new-agent notice.
    #[serde(default)]
    pub told: Vec<String>,
    #[serde(default)]
    pub last_scan_at: Option<u64>,
}

fn internal_dir() -> PathBuf {
    crate::mdm::utils::home_dir()
        .join(".autter")
        .join("internal")
}

fn agent_state_path() -> PathBuf {
    internal_dir().join("agent-hook-notices.json")
}

fn onboard_stamp_path() -> PathBuf {
    internal_dir().join("onboard-notice-at")
}

pub fn read_agent_state() -> AgentNoticeState {
    std::fs::read(agent_state_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_agent_state(state: &AgentNoticeState) {
    let path = agent_state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(state) {
        let _ = crate::mdm::utils::write_atomic(&path, &json);
    }
}

/// Record the agents the user turned down in onboarding, so no notice ever
/// names them. Agents picked this time are taken off the list.
pub fn record_onboarding_choice(declined: &[String], accepted: &[String]) {
    let mut state = read_agent_state();
    state.declined.retain(|id| !accepted.contains(id));
    for id in declined {
        if !state.declined.contains(id) {
            state.declined.push(id.clone());
        }
    }
    write_agent_state(&state);
}

/// Detected agents without autter hooks that the user has neither turned
/// down nor been told about, as `(id, name)`.
pub fn agents_to_announce(
    unhooked: &[(String, String)],
    state: &AgentNoticeState,
) -> Vec<(String, String)> {
    unhooked
        .iter()
        .filter(|(id, _)| !state.declined.contains(id) && !state.told.contains(id))
        .cloned()
        .collect()
}

pub fn new_agent_notice(name: &str) -> String {
    format!(
        "autter: {name} is installed, but its edits are not captured. Run `autter install-hooks` to add autter's hooks."
    )
}

/// Whether the (slow-ish) look for installed agents is due: once a day.
pub fn agent_scan_due(state: &AgentNoticeState, now: u64) -> bool {
    state
        .last_scan_at
        .is_none_or(|at| now.saturating_sub(at) >= DAY_SECS)
}

/// Agents with config-file hooks that are installed but have no autter hooks.
/// Editor extensions (VS Code, JetBrains, Visual Studio) are left out: asking
/// them means starting the editor's CLI, and `autter doctor` covers them.
fn unhooked_agents() -> Vec<(String, String)> {
    use crate::mdm::hook_installer::HookInstallerParams;
    let Ok(binary_path) = crate::mdm::utils::resolve_hook_binary_path() else {
        return Vec::new();
    };
    let params = HookInstallerParams { binary_path };
    crate::mdm::agents::get_all_installers()
        .into_iter()
        .filter(|installer| {
            installer.uses_config_hooks()
                && !matches!(installer.id(), "vscode" | "jetbrains" | "visual-studio")
        })
        .filter_map(|installer| match installer.check_hooks(&params) {
            Ok(check) if check.tool_installed && !check.hooks_installed => {
                Some((installer.id().to_string(), installer.name().to_string()))
            }
            _ => None,
        })
        .collect()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Print whichever setup notices are due for this command run.
pub fn maybe_print(args: &[String]) {
    if !notices_allowed(args, NoticeEnv::current()) {
        return;
    }
    let now = now_secs();
    let onboarded = crate::config::load_file_config_public()
        .ok()
        .and_then(|c| c.onboarding_completed)
        == Some(true);

    if !onboarded {
        let stamp = onboard_stamp_path();
        let last = std::fs::read_to_string(&stamp)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        if onboard_notice_due(false, last, now) {
            if let Some(parent) = stamp.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&stamp, now.to_string());
            eprintln!("{ONBOARD_NOTICE}");
        }
        // Onboarding asks about every agent; nothing more to say until then.
        return;
    }

    let mut state = read_agent_state();
    if agent_scan_due(&state, now) {
        state.last_scan_at = Some(now);
        let announce = agents_to_announce(&unhooked_agents(), &state);
        for (id, name) in &announce {
            eprintln!("{}", new_agent_notice(name));
            state.told.push(id.clone());
        }
        write_agent_state(&state);
    }

    crate::commands::upgrade::maybe_print_npm_update_notice();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person() -> NoticeEnv {
        NoticeEnv {
            stdout_tty: true,
            stderr_tty: true,
            ci: false,
            agent: false,
        }
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn notices_only_reach_a_person_on_everyday_commands() {
        assert!(notices_allowed(&args(&["stats"]), person()));
        assert!(notices_allowed(
            &args(&["-C", "/repo", "blame", "a.rs"]),
            person()
        ));
        assert!(notices_allowed(&args(&["status"]), person()));

        // Never on the commands where it is noise or already said.
        for command in [
            "onboard",
            "help",
            "--help",
            "--version",
            "-v",
            "version",
            "doctor",
            "debug",
            "install-hooks",
            "install",
            "uninstall",
            "upgrade",
            "login",
            "checkpoint",
            "memory-hook",
            "bg",
            "mcp",
        ] {
            assert!(!notices_allowed(&args(&[command]), person()), "{command}");
        }
        // Bare `autter` shows the home screen, which says it already.
        assert!(!notices_allowed(&args(&[]), person()));

        // Never with --json, a help flag, or hook input.
        assert!(!notices_allowed(&args(&["stats", "--json"]), person()));
        assert!(!notices_allowed(&args(&["--json", "stats"]), person()));
        assert!(!notices_allowed(&args(&["stats", "--help"]), person()));
        assert!(!notices_allowed(
            &args(&["stats", "--hook-input", "stdin"]),
            person()
        ));

        // Never in CI, from an agent, or without a terminal on both streams.
        for env in [
            NoticeEnv {
                ci: true,
                ..person()
            },
            NoticeEnv {
                agent: true,
                ..person()
            },
            NoticeEnv {
                stdout_tty: false,
                ..person()
            },
            NoticeEnv {
                stderr_tty: false,
                ..person()
            },
        ] {
            assert!(!notices_allowed(&args(&["stats"]), env), "{env:?}");
        }
    }

    #[test]
    fn onboarding_reminder_is_at_most_daily_and_stops_after_onboarding() {
        let now = 10 * DAY_SECS;
        assert!(onboard_notice_due(false, None, now));
        assert!(!onboard_notice_due(false, Some(now - 60), now));
        assert!(!onboard_notice_due(false, Some(now - DAY_SECS + 1), now));
        assert!(onboard_notice_due(false, Some(now - DAY_SECS), now));
        assert!(!onboard_notice_due(true, None, now));
        assert!(ONBOARD_NOTICE.contains("autter onboard"));
    }

    #[test]
    fn a_new_agent_is_named_once_and_a_declined_one_never() {
        let unhooked = vec![
            ("claude-code".to_string(), "Claude Code".to_string()),
            ("codex".to_string(), "Codex".to_string()),
            ("cursor".to_string(), "Cursor".to_string()),
        ];
        let state = AgentNoticeState {
            declined: vec!["cursor".to_string()],
            told: vec!["codex".to_string()],
            last_scan_at: None,
        };
        assert_eq!(
            agents_to_announce(&unhooked, &state),
            vec![("claude-code".to_string(), "Claude Code".to_string())]
        );
        let line = new_agent_notice("Claude Code");
        assert!(line.contains("Claude Code"), "{line}");
        assert!(line.contains("autter install-hooks"), "{line}");
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn installed_agents_are_looked_for_at_most_daily() {
        let now = 10 * DAY_SECS;
        let mut state = AgentNoticeState::default();
        assert!(agent_scan_due(&state, now));
        state.last_scan_at = Some(now - 3600);
        assert!(!agent_scan_due(&state, now));
        state.last_scan_at = Some(now - DAY_SECS);
        assert!(agent_scan_due(&state, now));
    }

    #[test]
    fn agent_notice_state_reads_older_or_partial_files() {
        let state: AgentNoticeState = serde_json::from_str(r#"{"told":["codex"]}"#).unwrap();
        assert_eq!(state.told, vec!["codex".to_string()]);
        assert!(state.declined.is_empty());
        assert_eq!(state.last_scan_at, None);
    }
}
