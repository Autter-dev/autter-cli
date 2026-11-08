use crate::commands::arg_parser::{paint, paint_err};
use crate::config;
use crate::daemon::DaemonConfig;
use crate::error::AutterError;
use crate::mdm::agents::get_all_installers;
use crate::mdm::hook_installer::HookInstallerParams;
use crate::mdm::skills_installer;
use crate::mdm::spinner::{Spinner, print_diff};
use crate::mdm::utils::resolve_hook_binary_path;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const TRACE2_EVENT_TARGET_KEY: &str = "trace2.eventTarget";
const TRACE2_EVENT_NESTING_KEY: &str = "trace2.eventNesting";
const TRACE2_EVENT_NESTING_VALUE: &str = "10";
const TRACE2_EVENT_BRIEF_KEY: &str = "trace2.eventBrief";
const TRACE2_ENV_VARS_KEY: &str = "trace2.envVars";
const VISUAL_STUDIO_INSTALLER_ID: &str = "visual-studio";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct InstallOptions {
    dry_run: bool,
    verbose: bool,
    install_skills: bool,
    include_visual_studio_extension: bool,
    /// When true, also write global git trace2 config and start the daemon.
    apply_system: bool,
    /// Only these installer ids, when set (the tools chosen in onboarding).
    only: Option<HashSet<String>>,
    /// Running inside onboarding's step rail: skip this command's own section
    /// header and "not detected" list, which onboarding already showed.
    on_rail: bool,
    /// Run directly by a person at a terminal: lay the tools out on a rail of
    /// their own, opened by a step line and closed with a summary.
    rail_layout: bool,
}

/// A tool autter can install hooks for, found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedTool {
    pub id: String,
    pub name: String,
}

/// The tools on this machine that `install-hooks` would set up. A tool whose
/// version check fails is still listed, so installing it reports why.
pub fn detect_tools() -> Vec<DetectedTool> {
    let Ok(binary_path) = resolve_hook_binary_path() else {
        return Vec::new();
    };
    let params = HookInstallerParams { binary_path };
    let options = InstallOptions::default();
    get_all_installers()
        .into_iter()
        .filter(|installer| should_include_installer(installer.id(), &options))
        .filter(|installer| match installer.check_hooks(&params) {
            Ok(check) => check.tool_installed,
            Err(_) => true,
        })
        .map(|installer| DetectedTool {
            id: installer.id().to_string(),
            name: installer.name().to_string(),
        })
        .collect()
}

/// Install hooks for `tool_ids` (from [`detect_tools`]; `None` for every
/// detected tool), plus the global git trace2 config and background service
/// when `system` is set. Output hangs off onboarding's step rail.
pub fn run_for_tools(
    tool_ids: Option<&[String]>,
    system: bool,
) -> Result<InstallOutcome, AutterError> {
    let _rail = crate::ui::progress::on_rail();
    run_with_options(InstallOptions {
        apply_system: system,
        only: tool_ids.map(|ids| ids.iter().cloned().collect()),
        on_rail: true,
        ..InstallOptions::default()
    })
}

/// Installation status for a tool
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStatus {
    /// Tool was not detected on the machine
    NotFound,
    /// Hooks/extensions were successfully installed or updated
    Installed,
    /// Hooks/extensions were already up to date
    AlreadyInstalled,
    /// Installation attempted but failed
    Failed,
}

impl InstallStatus {
    /// Convert status to string representation
    pub fn as_str(&self) -> &'static str {
        match self {
            InstallStatus::NotFound => "not_found",
            InstallStatus::Installed => "installed",
            InstallStatus::AlreadyInstalled => "already_installed",
            InstallStatus::Failed => "failed",
        }
    }
}

/// Detailed install result for metrics tracking
#[derive(Debug, Clone)]
pub struct InstallResult {
    pub status: InstallStatus,
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

impl InstallResult {
    pub fn installed() -> Self {
        Self {
            status: InstallStatus::Installed,
            error: None,
            warnings: Vec::new(),
        }
    }

    pub fn already_installed() -> Self {
        Self {
            status: InstallStatus::AlreadyInstalled,
            error: None,
            warnings: Vec::new(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: InstallStatus::NotFound,
            error: None,
            warnings: Vec::new(),
        }
    }

    pub fn failed(msg: impl Into<String>) -> Self {
        Self {
            status: InstallStatus::Failed,
            error: Some(msg.into()),
            warnings: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    /// Get message for ClickHouse (error if failed, else joined warnings)
    pub fn message_for_metrics(&self) -> Option<String> {
        if let Some(err) = &self.error {
            Some(err.clone())
        } else if !self.warnings.is_empty() {
            Some(self.warnings.join("; "))
        } else {
            None
        }
    }
}

/// Convert a HashMap of tool statuses to string keys and values
pub fn to_hashmap(statuses: HashMap<String, InstallStatus>) -> HashMap<String, String> {
    statuses
        .into_iter()
        .map(|(k, v)| (k, v.as_str().to_string()))
        .collect()
}

fn print_amp_plugins_note(installer_id: &str) {
    if installer_id == "amp" {
        println!("  Note: Amp plugins are experimental. Run amp with `PLUGINS=all amp`.");
    }
}

fn print_github_copilot_note(installer_id: &str) {
    if installer_id == "github-copilot" {
        println!(
            "  Note: covers Copilot CLI and VS Code's built-in Copilot agent. VS Code 1.109.3+ runs these hooks natively — restart VS Code after installing."
        );
        println!(
            "  Remote dev (SSH/WSL/devcontainers) runs hooks on the remote host: install autter and run `autter install-hooks` there too. Verify with `autter debug`."
        );
    }
}

/// Find PIDs of running processes that match any of the given process names.
/// Returns a list of (pid, process_name) tuples for each match found.
fn find_running_pids(process_names: &[&str]) -> Vec<(u32, String)> {
    if process_names.is_empty() {
        return vec![];
    }

    let output = {
        #[cfg(unix)]
        {
            Command::new("ps")
                .args(["axo", "pid,comm"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
        }
        #[cfg(windows)]
        {
            Command::new("tasklist")
                .args(["/FO", "CSV", "/NH"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
        }
    };

    let Ok(output) = output else {
        return vec![];
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results: Vec<(u32, String)> = Vec::new();

    for line in stdout.lines() {
        #[cfg(unix)]
        {
            let trimmed = line.trim();
            // ps output: "  PID COMM" — split on whitespace
            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let pid_str = parts.next().unwrap_or("").trim();
            let comm = parts.next().unwrap_or("").trim();
            // comm may be a full path; extract the basename
            let base = comm.rsplit('/').next().unwrap_or(comm);
            if let Ok(pid) = pid_str.parse::<u32>() {
                for &name in process_names {
                    if base.eq_ignore_ascii_case(name) {
                        results.push((pid, base.to_string()));
                        break;
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            // tasklist CSV: "Image Name","PID",...
            let fields: Vec<&str> = line.split(',').collect();
            if fields.len() >= 2 {
                let image = fields[0].trim_matches('"');
                let pid_str = fields[1].trim_matches('"');
                let base = image.strip_suffix(".exe").unwrap_or(image);
                if let Ok(pid) = pid_str.parse::<u32>() {
                    for &name in process_names {
                        if base.eq_ignore_ascii_case(name) {
                            results.push((pid, base.to_string()));
                            break;
                        }
                    }
                }
            }
        }
    }

    results
}

fn set_global_git_config_value(git_cmd: &str, key: &str, value: &str) -> Result<(), AutterError> {
    let status = Command::new(git_cmd)
        .args(["config", "--global", key, value])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(AutterError::Generic(format!(
            "failed to set global git config key '{}'",
            key
        )))
    }
}

fn ensure_global_git_config_dirs() -> Result<(), AutterError> {
    if let Ok(path) = std::env::var("GIT_CONFIG_GLOBAL") {
        let config_path = PathBuf::from(path);
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        fs::create_dir_all(home)?;
    }

    Ok(())
}

fn remove_global_git_config_section(git_cmd: &str, section: &str) -> Result<(), AutterError> {
    let status = Command::new(git_cmd)
        .args(["config", "--global", "--remove-section", section])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    // Exit code 128 means the section doesn't exist, which is fine.
    if status.success() || status.code() == Some(128) {
        Ok(())
    } else {
        Err(AutterError::Generic(format!(
            "failed to remove global git config section '{}'",
            section
        )))
    }
}

/// Point git's trace2 event stream at the background service. Returns whether
/// the global git config changed.
///
/// Merges into the user's `[trace2]` section instead of replacing it: only the
/// keys autter needs are written, and `trace2.eventBrief` (which strips fields
/// autter reads from the event stream) is removed. Other trace2 settings the
/// user has, such as `normalTarget` or `perfTarget`, are left alone.
fn configure_daemon_trace2(dry_run: bool) -> Result<bool, AutterError> {
    let runtime_config = config::Config::fresh();

    ensure_global_git_config_dirs()?;

    let daemon_config = DaemonConfig::from_env_or_default_paths()?;
    let event_target = daemon_config.trace2_event_target();

    if dry_run {
        return Ok(false);
    }

    let git_cmd = runtime_config.git_cmd();
    let mut changed = false;
    for (key, value) in [
        (TRACE2_EVENT_TARGET_KEY, event_target.as_str()),
        (TRACE2_EVENT_NESTING_KEY, TRACE2_EVENT_NESTING_VALUE),
    ] {
        if global_git_config_values(git_cmd, key) != [value.to_string()] {
            replace_global_git_config_value(git_cmd, key, value)?;
            changed = true;
        }
    }
    if !global_git_config_values(git_cmd, TRACE2_EVENT_BRIEF_KEY).is_empty() {
        unset_global_git_config_key(git_cmd, TRACE2_EVENT_BRIEF_KEY)?;
        changed = true;
    }
    // Ask git to report the background-agent detection variables of every command, so
    // the daemon decides "was this commit made by a cloud agent?" from the committing
    // shell rather than from the daemon's own (frozen) environment. Names the user
    // already lists are kept.
    let existing_env_vars = global_git_config_values(git_cmd, TRACE2_ENV_VARS_KEY);
    let merged_env_vars =
        crate::authorship::background_agent::merge_trace2_env_vars(&existing_env_vars);
    if existing_env_vars != [merged_env_vars.clone()] {
        replace_global_git_config_value(git_cmd, TRACE2_ENV_VARS_KEY, &merged_env_vars)?;
        changed = true;
    }
    Ok(changed)
}

/// Every value of a global git config key (empty when unset or unreadable).
fn global_git_config_values(git_cmd: &str, key: &str) -> Vec<String> {
    Command::new(git_cmd)
        .args(["config", "--global", "--get-all", key])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Set a global key to exactly one value, replacing any duplicates.
fn replace_global_git_config_value(
    git_cmd: &str,
    key: &str,
    value: &str,
) -> Result<(), AutterError> {
    let status = Command::new(git_cmd)
        .args(["config", "--global", "--replace-all", key, value])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(AutterError::Generic(format!(
            "failed to set global git config key '{key}'"
        )))
    }
}

fn unset_global_git_config_key(git_cmd: &str, key: &str) -> Result<(), AutterError> {
    let status = Command::new(git_cmd)
        .args(["config", "--global", "--unset-all", key])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    // Exit code 5 means the key isn't set, which is fine.
    if status.success() || status.code() == Some(5) {
        Ok(())
    } else {
        Err(AutterError::Generic(format!(
            "failed to unset global git config key '{key}'"
        )))
    }
}

/// Align Git's native notes rewrite/display with Autter's authorship namespace
/// so rebase/amend don't warn about a missing default notes ref.
fn configure_authorship_notes_refs(dry_run: bool) {
    if dry_run {
        return;
    }
    let runtime_config = config::Config::fresh();
    if let Err(e) = ensure_global_git_config_dirs() {
        eprintln!("[autter] warning: could not prepare git config dirs (non-fatal): {e}");
        return;
    }
    if let Err(e) = set_global_git_config_value(
        runtime_config.git_cmd(),
        "notes.rewriteRef",
        "refs/notes/ai",
    ) {
        eprintln!("[autter] warning: could not set notes.rewriteRef (non-fatal): {e}");
    }
    if let Err(e) = set_global_git_config_value(
        runtime_config.git_cmd(),
        "notes.displayRef",
        "refs/notes/ai",
    ) {
        eprintln!("[autter] warning: could not set notes.displayRef (non-fatal): {e}");
    }
}

/// (Re)start the background service so it picks up the freshly-written trace2
/// config. A failure is returned, not just printed, so callers never report
/// success after the service timed out.
fn ensure_daemon(dry_run: bool) -> Result<(), String> {
    if dry_run {
        return Ok(());
    }

    // Don't touch daemon inside test harnesses
    if std::env::var_os("AUTTER_TEST_DB_PATH").is_some() {
        return Ok(());
    }

    let daemon_config = DaemonConfig::from_env_or_default_paths()
        .map_err(|e| format!("could not locate the background service: {e}"))?;

    // Uses soft shutdown → hard kill escalation if needed.
    crate::commands::daemon::restart_daemon(&daemon_config)
}

/// Main entry point for install-hooks command.
///
/// By default this only installs IDE/agent hooks. Global git trace2 config and
/// daemon start are applied when `--system` is passed, or from `autter onboard`
/// after the user consents — so `npm install` / bare `install-hooks` no longer
/// mutate the machine silently.
pub fn run(args: &[String]) -> Result<HashMap<String, String>, AutterError> {
    let mut options = match parse_install_options(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    };
    // Verbose output interleaves diffs, which don't belong on the rail.
    options.rail_layout =
        !options.verbose && std::io::stdout().is_terminal() && crate::ui::caps().tty;
    let rail = options.rail_layout.then(crate::ui::progress::on_rail);
    let apply_system = options.apply_system && !options.dry_run;
    let mut outcome = run_with_options(options)?;
    drop(rail);

    if apply_system {
        // Report success for the system part only once capture is verified.
        if outcome.system_issues.is_empty() && !in_test_harness() {
            let spinner = crate::ui::progress::Spinner::new("Verifying git capture\u{2026}");
            outcome.system_issues = crate::diagnostics::verify_capture();
            spinner.stop();
        }
        if !outcome.system_issues.is_empty() {
            return Err(AutterError::Generic(format_system_issues(
                &outcome.system_issues,
            )));
        }
        eprintln!("{}", system_summary(outcome.trace2_changed));
    }
    Ok(outcome.statuses)
}

/// What one install run did.
#[derive(Debug, Default)]
pub struct InstallOutcome {
    /// Per-tool install status, as reported to metrics.
    pub statuses: HashMap<String, String>,
    /// Whether the global git trace2 config was written or changed.
    pub trace2_changed: bool,
    /// Why git capture or the background service could not be set up. Empty
    /// when the system part was not requested or succeeded.
    pub system_issues: Vec<crate::diagnostics::CaptureIssue>,
}

fn in_test_harness() -> bool {
    std::env::var_os("AUTTER_TEST_DB_PATH").is_some()
}

/// The closing line for a successful `install --system`, which must never
/// read as "nothing happened" when it wrote git config.
fn system_summary(trace2_changed: bool) -> String {
    if trace2_changed {
        "Git capture set up: wrote trace2 settings to your global git config, started the background service, and verified that commits reach it.".to_string()
    } else {
        "Git capture verified: trace2 settings were already in place and commits reach the background service.".to_string()
    }
}

/// Each capture problem with its fix, for the install error message.
fn format_system_issues(issues: &[crate::diagnostics::CaptureIssue]) -> String {
    let mut out =
        String::from("git capture is not working, so commits won't get authorship notes:");
    for issue in issues {
        out.push_str(&format!(
            "\n  \u{2022} {}\n    Fix: {}",
            issue.what, issue.fix
        ));
    }
    out.push_str("\nThen run `autter doctor` to confirm.");
    out
}

fn run_with_options(options: InstallOptions) -> Result<InstallOutcome, AutterError> {
    let mut outcome = InstallOutcome::default();
    if options.apply_system {
        // Daemon trace2 config must be in place before any install work starts.
        // A failure doesn't stop the hook install (the global git config may be
        // read-only, e.g. a Nix store symlink), but it is reported, never
        // swallowed into a success.
        match configure_daemon_trace2(options.dry_run) {
            Ok(changed) => outcome.trace2_changed = changed,
            Err(e) => outcome.system_issues.push(crate::diagnostics::CaptureIssue::new(
                format!("Could not write git's trace2 settings to your global git config ({e})"),
                "make sure your global git config (~/.gitconfig) is writable, then run `autter install --system`",
            )),
        }
        if let Err(e) = ensure_daemon(options.dry_run) {
            outcome
                .system_issues
                .push(crate::diagnostics::CaptureIssue::new(
                    format!("Could not start the background service ({e})"),
                    crate::commands::daemon::RESTART_COMMAND,
                ));
        }
    }

    // Always align Git notes refs with Autter's authorship namespace — even when
    // --system wasn't passed — so attribution works without notes warnings.
    configure_authorship_notes_refs(options.dry_run);

    // Now that the daemon is (re)started (when requested), initialize the telemetry
    // handle so that install-hooks metrics and observability events route through it.
    if !options.dry_run {
        let _ = crate::daemon::telemetry_handle::init_daemon_telemetry_handle();
    }

    // Hook configs need a binary path that outlives this process; prefer the
    // stable installed binary when running from a cargo build directory.
    let binary_path = resolve_hook_binary_path()?;
    persist_install_config(&binary_path, options.dry_run)?;
    let params = HookInstallerParams { binary_path };

    // Run async operations with smol and convert result
    let statuses = smol::block_on(async_run_install(&params, &options))?;

    // Clean up legacy envelope logs directory and related artifacts.
    // These are no longer used — all telemetry now routes through the daemon.
    if !options.dry_run {
        cleanup_legacy_envelope_logs();
    }

    outcome.statuses = to_hashmap(statuses);
    Ok(outcome)
}

/// Flags accepted by `autter uninstall`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct FullUninstallOptions {
    pub dry_run: bool,
    pub verbose: bool,
    /// Skip the confirmation prompt (`--yes`/`-y`).
    pub yes: bool,
}

/// Parse `autter uninstall` flags. Anything else is an error, so a mistyped
/// flag never runs the uninstall with defaults the user didn't ask for.
pub(crate) fn parse_full_uninstall_options(
    args: &[String],
) -> Result<FullUninstallOptions, String> {
    let mut options = FullUninstallOptions::default();
    for arg in args {
        match arg.as_str() {
            "--dry-run" | "--dry-run=true" => options.dry_run = true,
            "--dry-run=false" => options.dry_run = false,
            "--verbose" | "-v" => options.verbose = true,
            "--yes" | "-y" => options.yes = true,
            other => {
                return Err(crate::commands::arg_parser::unknown_option_message(
                    "uninstall",
                    other,
                ));
            }
        }
    }
    Ok(options)
}

/// What `autter uninstall` should do about confirmation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UninstallConsent {
    /// Go ahead: `--yes`, or a dry run that changes nothing.
    Proceed,
    /// Ask at the terminal first.
    Ask,
    /// No terminal to ask at and no `--yes`: refuse.
    Refuse,
}

pub(crate) fn uninstall_consent(
    options: &FullUninstallOptions,
    interactive: bool,
) -> UninstallConsent {
    if options.yes || options.dry_run {
        UninstallConsent::Proceed
    } else if interactive {
        UninstallConsent::Ask
    } else {
        UninstallConsent::Refuse
    }
}

fn confirm_full_uninstall() -> bool {
    use std::io::{BufRead, Write};
    eprintln!("This removes autter from this machine's tools:");
    eprintln!("  - hooks from every coding agent and editor autter set up");
    eprintln!("  - the editor extensions autter installed (VS Code, Cursor)");
    eprintln!("  - autter's global git trace2 settings");
    eprintln!("  - the background service (it is stopped)");
    eprintln!("The autter binary, PATH entries and git notes are kept.");
    eprint!("Uninstall? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Whether the global git config has a `[trace2]` section to remove.
fn global_git_config_has_section(git_cmd: &str, section: &str) -> bool {
    Command::new(git_cmd)
        .args([
            "config",
            "--global",
            "--get-regexp",
            &format!("^{}\\.", section),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Full uninstall: agent hooks and editor extensions, global trace2, and stop
/// the daemon. Asks first at a terminal; without one it needs `--yes`. Prints
/// a line only for what was actually removed.
pub fn run_full_uninstall(args: &[String]) -> Result<HashMap<String, String>, AutterError> {
    let options = match parse_full_uninstall_options(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    };

    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    match uninstall_consent(&options, interactive) {
        UninstallConsent::Proceed => {}
        UninstallConsent::Ask => {
            if !confirm_full_uninstall() {
                eprintln!("Uninstall cancelled. Nothing was changed.");
                std::process::exit(crate::commands::EXIT_USAGE_ERROR);
            }
        }
        UninstallConsent::Refuse => {
            eprintln!(
                "error: `autter uninstall` removes autter's hooks, editor extensions, git trace2 settings and background service."
            );
            eprintln!(
                "There's no terminal to confirm at, so nothing was changed. Re-run with --yes to uninstall, or --dry-run to see what would be removed."
            );
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    }

    let mut hook_args = Vec::new();
    if options.dry_run {
        hook_args.push("--dry-run".to_string());
    }
    if options.verbose {
        hook_args.push("--verbose".to_string());
    }
    let statuses = run_uninstall(&hook_args)?;

    let git_cmd = config::Config::fresh().git_cmd().to_string();
    let had_trace2 = global_git_config_has_section(&git_cmd, "trace2");
    let daemon_config = crate::daemon::DaemonConfig::from_env_or_default_paths().ok();
    let daemon_running = daemon_config
        .as_ref()
        .is_some_and(crate::commands::daemon::daemon_is_running_or_starting);

    let mut removed: Vec<&str> = Vec::new();
    if statuses
        .values()
        .any(|s| s == InstallStatus::Installed.as_str())
    {
        removed.push("IDE/agent hooks and editor extensions");
    }

    if options.dry_run {
        if had_trace2 {
            eprintln!("[dry-run] would remove the global git trace2 config");
        }
        if daemon_running {
            eprintln!("[dry-run] would stop the background service");
        }
        return Ok(statuses);
    }

    if had_trace2 {
        match remove_global_git_config_section(&git_cmd, "trace2") {
            Ok(()) => removed.push("global git trace2 config"),
            Err(e) => eprintln!("Warning: could not remove global trace2 config: {e}"),
        }
    }
    if daemon_running && let Some(daemon_config) = daemon_config.as_ref() {
        match crate::commands::daemon::stop_daemon(daemon_config, std::time::Duration::from_secs(5))
        {
            Ok(()) => removed.push("background service (stopped)"),
            Err(e) => eprintln!("Warning: could not stop background service: {e}"),
        }
    }

    if removed.is_empty() {
        eprintln!(
            "Nothing to uninstall: no autter hooks, trace2 config or running background service were found."
        );
    } else {
        eprintln!("Removed: {}.", removed.join(", "));
    }
    eprintln!("Binary and PATH entries were left in place. Delete them manually if desired.");

    Ok(statuses)
}

/// Parse `autter install-hooks` flags. Unknown arguments are an error, so a
/// typo (or a flag from a newer version) never runs a real install.
fn parse_install_options(args: &[String]) -> Result<InstallOptions, String> {
    let mut options = InstallOptions::default();

    for arg in args {
        match arg.as_str() {
            "--dry-run" | "--dry-run=true" => options.dry_run = true,
            "--dry-run=false" => options.dry_run = false,
            "--verbose" | "-v" => options.verbose = true,
            "--skills" => options.install_skills = true,
            "--visual-studio-extension" => options.include_visual_studio_extension = true,
            "--system" => options.apply_system = true,
            other => {
                return Err(crate::commands::arg_parser::unknown_option_message(
                    "install-hooks",
                    other,
                ));
            }
        }
    }

    Ok(options)
}

fn should_include_installer(id: &str, options: &InstallOptions) -> bool {
    if let Some(only) = &options.only {
        return only.contains(id);
    }
    options.include_visual_studio_extension || id != VISUAL_STUDIO_INSTALLER_ID
}

fn persist_install_config(binary_path: &Path, dry_run: bool) -> Result<bool, AutterError> {
    if dry_run {
        return Ok(false);
    }

    let api_base = std::env::var("API_BASE").ok().filter(|s| !s.is_empty());
    let api_key = std::env::var("API_KEY").ok().filter(|s| !s.is_empty());

    if api_base.is_none() && api_key.is_none() {
        return Ok(false);
    }

    let mut file_config = crate::config::load_file_config_public().map_err(AutterError::Generic)?;
    let mut changed = false;

    if let Some(ref api_base) = api_base
        && file_config.api_base_url.as_deref() != Some(api_base.as_str())
    {
        file_config.api_base_url = Some(api_base.clone());
        changed = true;
    }

    if let Some(ref api_key) = api_key
        && file_config.api_key.as_deref() != Some(api_key.as_str())
    {
        file_config.api_key = Some(api_key.clone());
        changed = true;
    }

    if api_base.is_some() {
        let git_path_missing = file_config
            .git_path
            .as_ref()
            .map(|value| value.trim().is_empty())
            .unwrap_or(true);
        if git_path_missing && let Some(git_path) = detect_install_git_path(binary_path) {
            file_config.git_path = Some(git_path);
            changed = true;
        }
    }

    if !changed {
        return Ok(false);
    }

    crate::config::save_file_config(&file_config).map_err(AutterError::Generic)?;
    Ok(true)
}

fn detect_install_git_path(binary_path: &Path) -> Option<String> {
    let install_dir = binary_path.parent()?;

    #[cfg(windows)]
    {
        parse_git_og_cmd_path(&fs::read_to_string(install_dir.join("git-og.cmd")).ok()?)
    }

    #[cfg(not(windows))]
    {
        let target = fs::read_link(install_dir.join("git-og")).ok()?;
        let resolved = if target.is_absolute() {
            target
        } else {
            install_dir.join(target)
        };
        Some(resolved.to_string_lossy().to_string())
    }
}

#[cfg(windows)]
fn parse_git_og_cmd_path(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let start = line.find('"')?;
        let rest = &line[start + 1..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    })
}

/// Parse `autter uninstall-hooks` flags into `(dry_run, verbose)`. Unknown
/// arguments are an error.
pub(crate) fn parse_uninstall_hooks_options(args: &[String]) -> Result<(bool, bool), String> {
    let mut dry_run = false;
    let mut verbose = false;
    for arg in args {
        match arg.as_str() {
            "--dry-run" | "--dry-run=true" => dry_run = true,
            "--dry-run=false" => dry_run = false,
            "--verbose" | "-v" => verbose = true,
            other => {
                return Err(crate::commands::arg_parser::unknown_option_message(
                    "uninstall-hooks",
                    other,
                ));
            }
        }
    }
    Ok((dry_run, verbose))
}

/// Main entry point for uninstall-hooks command
pub fn run_uninstall(args: &[String]) -> Result<HashMap<String, String>, AutterError> {
    let (dry_run, verbose) = match parse_uninstall_hooks_options(args) {
        Ok(flags) => flags,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    };

    // Match the path resolution used at install time so uninstall targets the
    // same hook entries.
    let binary_path = resolve_hook_binary_path()?;
    let params = HookInstallerParams { binary_path };

    // Run async operations with smol and convert result
    let statuses = smol::block_on(async_run_uninstall(&params, dry_run, verbose))?;
    Ok(to_hashmap(statuses))
}

async fn async_run_install(
    params: &HookInstallerParams,
    options: &InstallOptions,
) -> Result<HashMap<String, InstallStatus>, AutterError> {
    let mut any_checked = false;
    let mut has_changes = false;
    let mut statuses: HashMap<String, InstallStatus> = HashMap::new();
    // Track detailed results for metrics (tool_id, result)
    let mut detailed_results: Vec<(String, InstallResult)> = Vec::new();

    // === Coding Agents ===
    if options.rail_layout {
        eprintln!();
        crate::ui::rail::print_step("Coding agents", None);
    } else if !options.on_rail {
        println!("\n{}", paint("1", "Coding Agents"));
    }

    let installers = get_all_installers();
    let mut installed_tools: HashSet<String> = HashSet::new();
    // Track agents whose hooks were checked (updated or already up to date) for restart warnings
    let mut agents_for_restart: Vec<(String, Vec<String>)> = Vec::new();
    let mut not_detected: Vec<&str> = Vec::new();

    for installer in &installers {
        let name = installer.name();
        let id = installer.id();

        if !should_include_installer(id, options) {
            continue;
        }

        // Check if tool is installed and hooks status
        match installer.check_hooks(params) {
            Ok(check_result) => {
                if !check_result.tool_installed {
                    statuses.insert(id.to_string(), InstallStatus::NotFound);
                    detailed_results.push((id.to_string(), InstallResult::not_found()));
                    not_detected.push(name);
                    continue;
                }

                installed_tools.insert(id.to_string());
                any_checked = true;

                // Install/update hooks (only for tools that use config file hooks)
                if installer.uses_config_hooks() {
                    let spinner = Spinner::new(&format!("{}: checking hooks", name));
                    spinner.start();

                    match installer.install_hooks(params, options.dry_run) {
                        Ok(Some(diff)) => {
                            if options.dry_run {
                                spinner.pending(&format!("{}: Pending updates", name));
                            } else {
                                spinner.success(&format!("{}: Hooks updated", name));
                                print_amp_plugins_note(id);
                                print_github_copilot_note(id);
                            }
                            if options.verbose {
                                println!();
                                print_diff(&diff);
                            }
                            has_changes = true;
                            statuses.insert(id.to_string(), InstallStatus::Installed);
                            detailed_results.push((id.to_string(), InstallResult::installed()));

                            // Track this agent for restart detection (skip in dry-run)
                            if !options.dry_run {
                                let pnames: Vec<String> = installer
                                    .process_names()
                                    .iter()
                                    .map(|s| s.to_string())
                                    .collect();
                                if !pnames.is_empty() {
                                    agents_for_restart.push((name.to_string(), pnames));
                                }
                            }
                        }
                        Ok(None) => {
                            spinner.success(&format!("{}: Hooks already up to date", name));
                            print_amp_plugins_note(id);
                            print_github_copilot_note(id);
                            statuses.insert(id.to_string(), InstallStatus::AlreadyInstalled);
                            detailed_results
                                .push((id.to_string(), InstallResult::already_installed()));

                            // Hooks may be up to date on disk but the agent still needs a
                            // restart to load them — track for the restart warning below.
                            if !options.dry_run {
                                let pnames: Vec<String> = installer
                                    .process_names()
                                    .iter()
                                    .map(|s| s.to_string())
                                    .collect();
                                if !pnames.is_empty()
                                    && !agents_for_restart.iter().any(|(n, _)| n == name)
                                {
                                    agents_for_restart.push((name.to_string(), pnames));
                                }
                            }
                        }
                        Err(e) => {
                            let error_msg = e.to_string();
                            spinner.error(&format!("{}: Failed to update hooks", name));
                            eprintln!("  Error: {}", error_msg);
                            statuses.insert(id.to_string(), InstallStatus::NotFound);
                            detailed_results
                                .push((id.to_string(), InstallResult::failed(error_msg)));
                        }
                    }
                }

                // Install extras (extensions, git.path, etc.)
                match installer.install_extras(params, options.dry_run) {
                    Ok(results) => {
                        let mut extras_changed = false;
                        for result in results {
                            if result.changed {
                                has_changes = true;
                                extras_changed = true;
                            }
                            if result.changed && !options.dry_run {
                                let extra_spinner = Spinner::new(&result.message);
                                extra_spinner.start();
                                extra_spinner.success(&result.message);
                            } else if result.changed && options.dry_run {
                                let extra_spinner = Spinner::new(&result.message);
                                extra_spinner.start();
                                extra_spinner.pending(&result.message);
                            } else if result.message.contains("already") {
                                let extra_spinner = Spinner::new(&result.message);
                                extra_spinner.start();
                                extra_spinner.success(&result.message);
                            } else if result.message.contains("Unable")
                                || result.message.contains("manually")
                            {
                                let extra_spinner = Spinner::new(&result.message);
                                extra_spinner.start();
                                extra_spinner.pending(&result.message);
                            }
                            if options.verbose
                                && let Some(diff) = result.diff
                            {
                                println!();
                                print_diff(&diff);
                            }

                            // Capture warning-like messages for metrics
                            if (result.message.contains("Unable")
                                || result.message.contains("manually")
                                || result.message.contains("Failed"))
                                && let Some((_, detail)) = detailed_results
                                    .iter_mut()
                                    .find(|(tool_id, _)| tool_id == id)
                            {
                                detail.warnings.push(result.message.clone());
                            }
                        }

                        // Track restart detection for extras-only agents (e.g. JetBrains, VS Code)
                        if extras_changed
                            && !options.dry_run
                            && !agents_for_restart.iter().any(|(n, _)| n == name)
                        {
                            let pnames: Vec<String> = installer
                                .process_names()
                                .iter()
                                .map(|s| s.to_string())
                                .collect();
                            if !pnames.is_empty() {
                                agents_for_restart.push((name.to_string(), pnames));
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("  Error installing extras for {}: {}", name, e);
                        // Capture extras error as a warning on the tool's result
                        if let Some((_, detail)) = detailed_results
                            .iter_mut()
                            .find(|(tool_id, _)| tool_id == id)
                        {
                            detail.warnings.push(format!("Extras install error: {}", e));
                        }
                    }
                }
            }
            Err(version_error) => {
                let error_msg = version_error.to_string();
                any_checked = true;
                let spinner = Spinner::new(&format!("{}: checking version", name));
                spinner.start();
                spinner.error(&format!("{}: Version check failed", name));
                eprintln!("  Error: {}", error_msg);
                eprintln!("  Please update {} to continue using autter hooks", name);
                statuses.insert(id.to_string(), InstallStatus::NotFound);
                detailed_results.push((id.to_string(), InstallResult::failed(error_msg)));
            }
        }
    }

    if options.rail_layout {
        if !not_detected.is_empty() {
            crate::ui::rail::print_note(&format!("Not detected: {}", not_detected.join(", ")));
        }
        let ready = statuses
            .values()
            .filter(|status| {
                matches!(
                    status,
                    InstallStatus::Installed | InstallStatus::AlreadyInstalled
                )
            })
            .count();
        crate::ui::rail::print_end(&match ready {
            0 => "No coding tools set up".to_string(),
            1 => "1 coding tool set up".to_string(),
            n => format!("{n} coding tools set up"),
        });
    } else if !not_detected.is_empty() && !options.on_rail {
        println!(
            "{}",
            paint("2", &format!("  Not detected: {}", not_detected.join(", ")))
        );
    }

    if options.install_skills {
        if let Ok(result) =
            skills_installer::install_skills(options.dry_run, options.verbose, &installed_tools)
            && result.changed
        {
            has_changes = true;
        }
    } else if let Ok(result) = skills_installer::uninstall_skills(options.dry_run, options.verbose)
        && result.changed
    {
        has_changes = true;
    }

    if !any_checked {
        if !options.on_rail {
            if options.apply_system {
                // `--system` still writes git capture settings and starts the
                // background service, so "Nothing to install" would be false.
                println!("No coding agents detected, so no agent hooks were installed.");
            } else {
                println!(
                    "No compatible IDEs or agent configurations detected. Nothing to install."
                );
            }
        }
    } else if has_changes && options.dry_run {
        println!(
            "\n{}",
            paint("33", "⚠ Dry-run mode (default). No changes were made.")
        );
        println!("To apply these changes, run:");
        println!("{}", paint("1", "  autter install-hooks --dry-run=false"));
    }

    // Warn when agents that use hooks are running — they must be restarted for
    // attribution to take effect, including when hooks were already up to date.
    if !options.dry_run && !agents_for_restart.is_empty() {
        let mut any_running = false;

        for (agent_name, pnames) in &agents_for_restart {
            let refs: Vec<&str> = pnames.iter().map(|s| s.as_str()).collect();
            let pids = find_running_pids(&refs);
            if !pids.is_empty() {
                if !any_running {
                    println!(
                        "\n{}",
                        paint(
                            "33",
                            "⚠ The following agents are currently running and must be restarted:"
                        )
                    );
                    any_running = true;
                }
                let pid_list: Vec<String> = pids.iter().map(|(pid, _)| pid.to_string()).collect();
                println!(
                    "  {} (PID: {})",
                    paint("1", agent_name),
                    pid_list.join(", ")
                );
            }
        }

        if any_running {
            println!();
            println!(
                "{}",
                paint(
                    "33",
                    "Restart the agents listed above for autter attribution to take effect."
                )
            );
            println!(
                "Any work done before installing autter (or before restarting) will be attributed as human."
            );
            println!(
                "This is expected — once you commit and start a fresh session, attribution will work correctly."
            );
            println!(
                "If the issue persists, run 'autter doctor' (or 'autter debug' on older versions)."
            );
        } else if !agents_for_restart.is_empty() {
            println!();
            println!(
                "{}",
                paint(
                    "33",
                    "If any coding agent was open during hook setup, restart it now for AI attribution to take effect."
                )
            );
        }
    }

    // Emit metrics for each agent/git_client result (only if not dry-run)
    if !options.dry_run {
        emit_install_hooks_metrics(&detailed_results);
    }

    // Warn if git version is below the minimum required for full functionality
    warn_if_git_version_too_old();

    Ok(statuses)
}

/// Minimum git version required for autter to function correctly.
/// git 2.22.0 introduced `git worktree list --porcelain` output format improvements
/// and trace2 event logging used by autter for attribution.
const MIN_GIT_VERSION: (u32, u32, u32) = (2, 22, 0);

/// Parse a git version string like "git version 2.39.1" into (major, minor, patch).
fn parse_git_version(output: &str) -> Option<(u32, u32, u32)> {
    // Strip the "git version " prefix and any platform suffix (e.g. "(Apple Git-140)")
    let version_str = output.trim().strip_prefix("git version ")?;
    let version_str = version_str.split_whitespace().next()?;
    let mut parts = version_str.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Print a loud warning if the installed git version is older than MIN_GIT_VERSION.
fn warn_if_git_version_too_old() {
    let output = Command::new("git")
        .args(["--version"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    let version = match output {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            parse_git_version(&text)
        }
        Err(_) => {
            eprintln!();
            eprintln!(
                "{}",
                paint_err(
                    "1;31",
                    "WARNING: git not found — autter requires git to function."
                )
            );
            eprintln!("Install git 2.22+ and re-run: autter install-hooks");
            eprintln!();
            return;
        }
    };

    if let Some(v) = version {
        let (maj, min, patch) = MIN_GIT_VERSION;
        if v < (maj, min, patch) {
            let (vmaj, vmin, vpatch) = v;
            eprintln!();
            eprintln!(
                "{}",
                paint_err(
                    "1;31",
                    "╔══════════════════════════════════════════════════════════════╗"
                )
            );
            eprintln!(
                "{}",
                paint_err(
                    "1;31",
                    "║  WARNING: git version too old — autter will not work         ║"
                )
            );
            eprintln!(
                "{}",
                paint_err(
                    "1;31",
                    "╚══════════════════════════════════════════════════════════════╝"
                )
            );
            eprintln!(
                "{}",
                paint_err(
                    "1;31",
                    &format!(
                        "Detected git {}.{}.{} — autter requires git >= {}.{}.{}",
                        vmaj, vmin, vpatch, maj, min, patch
                    )
                )
            );
            eprintln!(
                "{}",
                paint_err("33", "Please upgrade git before using autter:")
            );
            eprintln!("  macOS:   brew install git");
            eprintln!(
                "  Ubuntu:  sudo add-apt-repository ppa:git-core/ppa && sudo apt-get update && sudo apt-get install git"
            );
            eprintln!("  Windows: https://git-scm.com/download/win");
            eprintln!();
        }
    }
}

/// Emit metrics events for install-hooks results
fn emit_install_hooks_metrics(results: &[(String, InstallResult)]) {
    use crate::metrics::{EventAttributes, InstallHooksValues};

    let attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"));

    for (tool_id, result) in results {
        let mut values = InstallHooksValues::new()
            .tool_id(tool_id.clone())
            .status(result.status.as_str().to_string());

        if let Some(msg) = result.message_for_metrics() {
            values = values.message(msg);
        } else {
            values = values.message_null();
        }

        crate::metrics::record(values, attrs.clone());
    }
}

async fn async_run_uninstall(
    params: &HookInstallerParams,
    dry_run: bool,
    verbose: bool,
) -> Result<HashMap<String, InstallStatus>, AutterError> {
    let mut any_checked = false;
    let mut has_changes = false;
    let mut statuses: HashMap<String, InstallStatus> = HashMap::new();

    // Uninstall skills first (these are global, not per-agent, silently)
    if let Ok(result) = skills_installer::uninstall_skills(dry_run, verbose) {
        if result.changed {
            has_changes = true;
            statuses.insert("skills".to_string(), InstallStatus::Installed);
        } else {
            statuses.insert("skills".to_string(), InstallStatus::AlreadyInstalled);
        }
    }

    // === Coding Agents ===
    println!("\n{}", paint("1", "Coding Agents"));

    let installers = get_all_installers();

    for installer in installers {
        let name = installer.name();
        let id = installer.id();

        // Check if tool is installed
        match installer.check_hooks(params) {
            Ok(check_result) => {
                if !check_result.tool_installed {
                    statuses.insert(id.to_string(), InstallStatus::NotFound);
                    continue;
                }

                let hooks_were_installed = check_result.hooks_installed;
                if !hooks_were_installed {
                    statuses.insert(id.to_string(), InstallStatus::NotFound);
                } else {
                    any_checked = true;

                    // Uninstall hooks
                    let spinner = Spinner::new(&format!("{}: removing hooks", name));
                    spinner.start();

                    match installer.uninstall_hooks(params, dry_run) {
                        Ok(Some(diff)) => {
                            if dry_run {
                                spinner.pending(&format!("{}: Pending removal", name));
                            } else {
                                spinner.success(&format!("{}: Hooks removed", name));
                            }
                            if verbose {
                                println!();
                                print_diff(&diff);
                            }
                            has_changes = true;
                            statuses.insert(id.to_string(), InstallStatus::Installed);
                        }
                        Ok(None) => {
                            spinner.success(&format!("{}: No hooks to remove", name));
                            statuses.insert(id.to_string(), InstallStatus::AlreadyInstalled);
                        }
                        Err(e) => {
                            spinner.error(&format!("{}: Failed to remove hooks", name));
                            eprintln!("  Error: {}", e);
                            statuses.insert(id.to_string(), InstallStatus::NotFound);
                        }
                    }
                }

                // Uninstall extras (editor extensions etc.) even when the hook
                // config is already gone: install may have added them without
                // hooks, and leaving them behind was a reported bug.
                match installer.uninstall_extras(params, dry_run) {
                    Ok(results) => {
                        for result in results {
                            if result.changed {
                                has_changes = true;
                                any_checked = true;
                                statuses.insert(id.to_string(), InstallStatus::Installed);
                            }
                            // A tool whose hooks were already gone only
                            // gets a line when something was removed.
                            if !result.message.is_empty()
                                && (result.changed || hooks_were_installed)
                            {
                                let extra_spinner = Spinner::new(&result.message);
                                extra_spinner.start();
                                if result.changed {
                                    extra_spinner.success(&result.message);
                                } else {
                                    extra_spinner.pending(&result.message);
                                }
                            }
                            if verbose && let Some(diff) = result.diff {
                                println!();
                                print_diff(&diff);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("  Error uninstalling extras for {}: {}", name, e);
                    }
                }
            }
            Err(e) => {
                eprintln!("  Error checking {}: {}", name, e);
                statuses.insert(id.to_string(), InstallStatus::NotFound);
            }
        }
    }

    // Only report what actually changed: a run that found nothing says so.
    if !has_changes {
        if any_checked {
            println!("Nothing to remove: autter's hooks were already gone.");
        } else {
            println!("No autter hooks found to uninstall.");
        }
    } else if dry_run {
        println!("\n{}", paint("33", "⚠ Dry run. No changes were made."));
        println!("Run the same command without --dry-run to apply these changes.");
    } else {
        println!("Removed autter's hooks from the tools listed above.");
    }

    Ok(statuses)
}

/// Remove the legacy envelope logs directory and related lock/marker files.
///
/// All telemetry now flows through the daemon control socket, so the per-PID
/// log file system under `~/.autter/internal/logs/` is no longer needed.
fn cleanup_legacy_envelope_logs() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let internal = home.join(".autter").join("internal");

    // Remove the entire logs directory
    let logs_dir = internal.join("logs");
    if logs_dir.is_dir() {
        let _ = fs::remove_dir_all(&logs_dir);
    }

    // Remove the flush-logs lock file
    let _ = fs::remove_file(internal.join("flush-logs.lock"));

    // Remove the debounce marker file
    let _ = fs::remove_file(internal.join("last_flush_trigger_ts"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;
    use tempfile::tempdir;

    struct EnvVarGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            // SAFETY: tests marked `serial` avoid concurrent env mutation.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            // SAFETY: tests marked `serial` avoid concurrent env mutation.
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, old }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: tests marked `serial` avoid concurrent env mutation.
            unsafe {
                if let Some(old) = &self.old {
                    std::env::set_var(self.key, old);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn test_binary_path(install_dir: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            install_dir.join("autter.exe")
        }

        #[cfg(not(windows))]
        {
            install_dir.join("autter")
        }
    }

    fn write_install_git_marker(install_dir: &Path, git_path: &str) {
        #[cfg(windows)]
        {
            fs::write(
                install_dir.join("git-og.cmd"),
                format!("@echo off\r\n\"{}\" %*\r\n", git_path),
            )
            .unwrap();
        }

        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink(git_path, install_dir.join("git-og")).unwrap();
        }
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_install_options_rejects_unknown_args() {
        // `install --foo` used to run a real install with the flag ignored.
        let err = parse_install_options(&strings(&["--sytem"]))
            .err()
            .expect("unknown flag must be rejected");
        assert!(err.contains("unknown option '--sytem'"), "{err}");
        assert!(err.contains("autter install-hooks --help"), "{err}");
        assert!(parse_install_options(&strings(&["extra"])).is_err());
    }

    #[test]
    fn parse_uninstall_hooks_options_rejects_unknown_args() {
        assert_eq!(
            parse_uninstall_hooks_options(&strings(&["--dry-run", "-v"])).unwrap(),
            (true, true)
        );
        assert_eq!(
            parse_uninstall_hooks_options(&strings(&["--dry-run=false"])).unwrap(),
            (false, false)
        );
        assert!(parse_uninstall_hooks_options(&strings(&["--force"])).is_err());
    }

    #[test]
    fn parse_full_uninstall_options_accepts_yes_and_rejects_unknown() {
        let options =
            parse_full_uninstall_options(&strings(&["--yes", "--dry-run", "--verbose"])).unwrap();
        assert!(options.yes && options.dry_run && options.verbose);
        assert!(parse_full_uninstall_options(&strings(&["-y"])).unwrap().yes);
        let err = parse_full_uninstall_options(&strings(&["--all"]))
            .err()
            .expect("unknown flag must be rejected");
        assert!(err.contains("autter uninstall --help"), "{err}");
    }

    #[test]
    fn uninstall_needs_confirmation_or_yes() {
        let plain = FullUninstallOptions::default();
        assert_eq!(uninstall_consent(&plain, true), UninstallConsent::Ask);
        // No terminal and no --yes: refuse rather than uninstall silently.
        assert_eq!(uninstall_consent(&plain, false), UninstallConsent::Refuse);
        let yes = FullUninstallOptions {
            yes: true,
            ..Default::default()
        };
        assert_eq!(uninstall_consent(&yes, false), UninstallConsent::Proceed);
        assert_eq!(uninstall_consent(&yes, true), UninstallConsent::Proceed);
        // A dry run changes nothing, so it never needs confirmation.
        let dry = FullUninstallOptions {
            dry_run: true,
            ..Default::default()
        };
        assert_eq!(uninstall_consent(&dry, false), UninstallConsent::Proceed);
    }

    #[test]
    fn parse_install_options_defaults_visual_studio_extension_to_disabled() {
        let options = parse_install_options(&[]).unwrap();

        assert!(!options.include_visual_studio_extension);
        assert!(!should_include_installer(
            VISUAL_STUDIO_INSTALLER_ID,
            &options
        ));
        assert!(should_include_installer("vscode", &options));
    }

    #[test]
    fn parse_install_options_enables_visual_studio_extension_flag() {
        let args = vec![
            "--dry-run".to_string(),
            "--visual-studio-extension".to_string(),
            "--skills".to_string(),
            "-v".to_string(),
        ];
        let options = parse_install_options(&args).unwrap();

        assert!(options.dry_run);
        assert!(options.verbose);
        assert!(options.install_skills);
        assert!(options.include_visual_studio_extension);
        assert!(should_include_installer(
            VISUAL_STUDIO_INSTALLER_ID,
            &options
        ));
    }

    #[test]
    #[serial]
    fn persist_install_config_updates_api_base_and_backfills_git_path() {
        let temp = tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(test_binary_path(&install_dir), "").unwrap();

        let expected_git_path = if cfg!(windows) {
            r"C:\Program Files\Git\bin\git.exe"
        } else {
            "/opt/custom/bin/git"
        };
        write_install_git_marker(&install_dir, expected_git_path);

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        #[cfg(windows)]
        let _userprofile = EnvVarGuard::set("USERPROFILE", temp.path().to_str().unwrap());
        let _api_base = EnvVarGuard::set("API_BASE", "https://enterprise.example");
        let _api_key = EnvVarGuard::remove("API_KEY");

        let changed = persist_install_config(&test_binary_path(&install_dir), false).unwrap();

        assert!(changed);

        let config = crate::config::load_file_config_public().unwrap();
        assert_eq!(
            config.api_base_url.as_deref(),
            Some("https://enterprise.example")
        );
        assert_eq!(config.git_path.as_deref(), Some(expected_git_path));
        assert_eq!(config.api_key, None);
    }

    #[test]
    #[serial]
    fn persist_install_config_preserves_existing_git_path() {
        let temp = tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(test_binary_path(&install_dir), "").unwrap();
        write_install_git_marker(
            &install_dir,
            if cfg!(windows) {
                r"C:\Program Files\Git\bin\git.exe"
            } else {
                "/opt/custom/bin/git"
            },
        );

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        #[cfg(windows)]
        let _userprofile = EnvVarGuard::set("USERPROFILE", temp.path().to_str().unwrap());
        let _api_base = EnvVarGuard::set("API_BASE", "https://enterprise.example");
        let _api_key = EnvVarGuard::remove("API_KEY");

        let existing_git_path = if cfg!(windows) {
            r"D:\PortableGit\bin\git.exe"
        } else {
            "/usr/local/bin/git"
        };
        crate::config::save_file_config(&crate::config::FileConfig {
            git_path: Some(existing_git_path.to_string()),
            ..Default::default()
        })
        .unwrap();

        persist_install_config(&test_binary_path(&install_dir), false).unwrap();

        let config = crate::config::load_file_config_public().unwrap();
        assert_eq!(
            config.api_base_url.as_deref(),
            Some("https://enterprise.example")
        );
        assert_eq!(config.git_path.as_deref(), Some(existing_git_path));
    }

    #[test]
    #[serial]
    fn persist_install_config_skips_without_env_or_in_dry_run() {
        let temp = tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(test_binary_path(&install_dir), "").unwrap();

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        #[cfg(windows)]
        let _userprofile = EnvVarGuard::set("USERPROFILE", temp.path().to_str().unwrap());
        let _api_base = EnvVarGuard::remove("API_BASE");
        let _api_key = EnvVarGuard::remove("API_KEY");

        let changed = persist_install_config(&test_binary_path(&install_dir), false).unwrap();
        assert!(!changed);
        assert!(!temp.path().join(".autter").join("config.json").exists());

        let _api_base = EnvVarGuard::set("API_BASE", "https://enterprise.example");
        let changed = persist_install_config(&test_binary_path(&install_dir), true).unwrap();
        assert!(!changed);
        assert!(!temp.path().join(".autter").join("config.json").exists());
    }

    #[test]
    #[serial]
    fn persist_install_config_persists_api_key() {
        let temp = tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(test_binary_path(&install_dir), "").unwrap();

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        #[cfg(windows)]
        let _userprofile = EnvVarGuard::set("USERPROFILE", temp.path().to_str().unwrap());
        let _api_base = EnvVarGuard::remove("API_BASE");
        let _api_key = EnvVarGuard::set("API_KEY", "sk-enterprise-key-12345");

        let changed = persist_install_config(&test_binary_path(&install_dir), false).unwrap();

        assert!(changed);

        let config = crate::config::load_file_config_public().unwrap();
        assert_eq!(config.api_key.as_deref(), Some("sk-enterprise-key-12345"));
        assert_eq!(config.api_base_url, None);
    }

    #[test]
    #[serial]
    fn persist_install_config_persists_both_api_base_and_api_key() {
        let temp = tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(test_binary_path(&install_dir), "").unwrap();

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        #[cfg(windows)]
        let _userprofile = EnvVarGuard::set("USERPROFILE", temp.path().to_str().unwrap());
        let _api_base = EnvVarGuard::set("API_BASE", "https://enterprise.example");
        let _api_key = EnvVarGuard::set("API_KEY", "sk-enterprise-key-12345");

        let changed = persist_install_config(&test_binary_path(&install_dir), false).unwrap();

        assert!(changed);

        let config = crate::config::load_file_config_public().unwrap();
        assert_eq!(
            config.api_base_url.as_deref(),
            Some("https://enterprise.example")
        );
        assert_eq!(config.api_key.as_deref(), Some("sk-enterprise-key-12345"));
    }

    #[cfg(windows)]
    #[test]
    fn parse_git_og_cmd_path_extracts_wrapped_git_path() {
        assert_eq!(
            parse_git_og_cmd_path("@echo off\r\n\"C:\\Program Files\\Git\\bin\\git.exe\" %*\r\n"),
            Some("C:\\Program Files\\Git\\bin\\git.exe".to_string())
        );
    }

    #[test]
    fn parse_git_version_standard() {
        assert_eq!(parse_git_version("git version 2.39.1"), Some((2, 39, 1)));
    }

    #[test]
    fn parse_git_version_apple_suffix() {
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
    }

    #[test]
    fn parse_git_version_no_patch() {
        assert_eq!(parse_git_version("git version 2.22"), Some((2, 22, 0)));
    }

    #[test]
    fn parse_git_version_old() {
        assert_eq!(parse_git_version("git version 2.17.1"), Some((2, 17, 1)));
        assert!(parse_git_version("git version 2.17.1").unwrap() < MIN_GIT_VERSION);
    }

    #[test]
    fn parse_git_version_at_minimum() {
        assert_eq!(parse_git_version("git version 2.22.0"), Some((2, 22, 0)));
        assert!(parse_git_version("git version 2.22.0").unwrap() >= MIN_GIT_VERSION);
    }

    #[test]
    fn parse_git_version_invalid() {
        assert_eq!(parse_git_version("not a git version"), None);
        assert_eq!(parse_git_version(""), None);
    }
}
