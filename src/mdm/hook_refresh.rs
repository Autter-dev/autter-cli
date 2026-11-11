//! Keep agent hooks in step with the installed CLI.
//!
//! Agent hook configs name an autter binary and a checkpoint preset. They are
//! written by `autter install-hooks`, and before this module nothing rewrote
//! them after a CLI upgrade, so a hook could keep calling a binary that moved
//! or a preset spelled the way an older CLI spelled it.
//!
//! - [`maybe_refresh_after_upgrade`] runs on every direct `autter` command. It
//!   compares the CLI version with the one recorded in
//!   `~/.autter/internal/hooks_version.json` and, when they differ, starts
//!   `autter install-hooks --refresh-stale` in the background (checking every
//!   agent can take seconds, too slow for a hook call).
//! - [`refresh_stale_hooks`] rewrites hooks only for agents that already have
//!   autter hooks installed (the user opted in) and are out of date. Agents
//!   with no autter hooks, never set up or declined, are not touched.
//! - [`command_problem`] tells `autter doctor` whether an out-of-date hook
//!   command will fail outright (binary gone, preset unknown) or only differs
//!   cosmetically.

use crate::error::AutterError;
use crate::mdm::hook_installer::{HookInstaller, HookInstallerParams};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `autter install-hooks` flag that refreshes out-of-date hooks only.
pub const REFRESH_STALE_FLAG: &str = "--refresh-stale";

/// Set in the background refresh worker so it never starts another one.
const WORKER_GUARD_ENV: &str = "AUTTER_HOOK_REFRESH_WORKER";

/// A second command within this window does not start another worker.
const CLAIM_TTL_SECS: u64 = 10 * 60;

/// What the last refresh did, for this CLI version.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookRefreshState {
    /// CLI version that last refreshed (or installed) hooks.
    pub version: String,
    #[serde(default)]
    pub refreshed_at: u64,
    /// Agents whose hooks could not be rewritten, with the error.
    #[serde(default)]
    pub failed: Vec<RefreshFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefreshFailure {
    pub id: String,
    pub name: String,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct RefreshOutcome {
    /// Names of agents whose hooks were rewritten.
    pub refreshed: Vec<String>,
    pub failed: Vec<RefreshFailure>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn state_path() -> Option<PathBuf> {
    crate::config::internal_dir_path().map(|dir| dir.join("hooks_version.json"))
}

fn claim_path() -> Option<PathBuf> {
    crate::config::internal_dir_path().map(|dir| dir.join("hooks_refresh_claim.json"))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AutterError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::mdm::utils::write_atomic(path, &serde_json::to_vec_pretty(value)?)
}

pub fn read_state() -> Option<HookRefreshState> {
    read_json(&state_path()?)
}

/// Record that hooks match this CLI version, with any agents that failed.
pub fn record_refresh(failed: Vec<RefreshFailure>) {
    let Some(path) = state_path() else { return };
    let state = HookRefreshState {
        version: env!("CARGO_PKG_VERSION").to_string(),
        refreshed_at: now_secs(),
        failed,
    };
    if let Err(e) = write_json(&path, &state) {
        tracing_debug(&format!("could not record hook refresh: {e}"));
    }
}

fn tracing_debug(message: &str) {
    if std::env::var_os("AUTTER_DEBUG").is_some() {
        eprintln!("[autter] {message}");
    }
}

/// Whether these arguments must not start a refresh. The subcommand is read
/// past any leading global flags, the way `handle_autter` reads it, so
/// `autter --json uninstall-hooks` is skipped like `autter uninstall-hooks`
/// (a refresh running beside it would rewrite the files it is removing hooks
/// from). Arguments that ask for help or do not parse start none either.
fn args_skip_refresh(args: &[String]) -> bool {
    use crate::commands::arg_parser::{ScanMode, pre_parse};
    match pre_parse(args, ScanMode::LeadingOnly, true) {
        Ok(parsed) => {
            parsed.flags.help || command_skips_refresh(parsed.rest.first().map(String::as_str))
        }
        Err(_) => true,
    }
}

/// Commands that must not start a refresh: they install or remove hooks
/// themselves, replace the binary, or are read by scripts right after install.
fn command_skips_refresh(subcommand: Option<&str>) -> bool {
    matches!(
        subcommand,
        None | Some(
            "install-hooks"
                | "install"
                | "uninstall-hooks"
                | "uninstall"
                | "upgrade"
                | "onboard"
                | "help"
                | "--help"
                | "-h"
                | "version"
                | "--version"
                | "-v"
        )
    )
}

#[derive(Serialize, Deserialize)]
struct RefreshClaim {
    version: String,
    at: u64,
}

/// On the first command after the CLI version changes, refresh the hooks of
/// agents the user already set up, in a background `autter install-hooks
/// --refresh-stale`. Costs one small file read when hooks are current.
pub fn maybe_refresh_after_upgrade(args: &[String]) {
    if std::env::var(WORKER_GUARD_ENV).as_deref() == Ok("1") || args_skip_refresh(args) {
        return;
    }
    // Integration tests run hundreds of commands against throwaway homes; they
    // opt in where the refresh is what they test.
    #[cfg(feature = "test-support")]
    if std::env::var("AUTTER_TEST_HOOK_REFRESH").as_deref() != Ok("1") {
        return;
    }

    let current = env!("CARGO_PKG_VERSION");
    if read_state().is_some_and(|s| s.version == current) {
        return;
    }
    let Some(claim) = claim_path() else { return };
    let now = now_secs();
    if read_json::<RefreshClaim>(&claim)
        .is_some_and(|c| c.version == current && now.saturating_sub(c.at) < CLAIM_TTL_SECS)
    {
        return;
    }
    if write_json(
        &claim,
        &RefreshClaim {
            version: current.to_string(),
            at: now,
        },
    )
    .is_err()
    {
        return;
    }
    crate::utils::spawn_internal_autter_subcommand(
        "install-hooks",
        &[REFRESH_STALE_FLAG],
        WORKER_GUARD_ENV,
        &[],
    );
}

/// Whether the agent's hook config is exactly what `install_hooks` would write
/// for this binary: a dry run that reports no change. Agents whose own check
/// only looks for an autter checkpoint command, whatever binary it names, use
/// this so a hook that still names a binary that moved (a nix store path, a
/// dev build, a renamed home) counts as out of date and gets rewritten.
pub fn install_would_change_nothing(
    installer: &dyn HookInstaller,
    params: &HookInstallerParams,
) -> bool {
    matches!(installer.install_hooks(params, true), Ok(None))
}

/// Installers whose hooks autter rewrites in place. Editor extensions (VS
/// Code, JetBrains, Visual Studio) are installed through the editor and are
/// left to `autter install-hooks`.
fn refreshable(installer: &dyn HookInstaller) -> bool {
    installer.uses_config_hooks() && installer.id() != "vscode"
}

/// Rewrite hooks for agents whose autter hooks are installed but out of date.
/// Agents without autter hooks are skipped, so nothing is installed for an
/// agent the user did not opt into.
pub fn refresh_stale_hooks(
    installers: &[Box<dyn HookInstaller>],
    params: &HookInstallerParams,
) -> RefreshOutcome {
    let mut outcome = RefreshOutcome::default();
    for installer in installers.iter().filter(|i| refreshable(i.as_ref())) {
        let Ok(check) = installer.check_hooks(params) else {
            continue;
        };
        if !(check.tool_installed && check.hooks_installed) || check.hooks_up_to_date {
            continue;
        }
        match installer.install_hooks(params, false) {
            Ok(_) => outcome.refreshed.push(installer.name().to_string()),
            Err(e) => outcome.failed.push(RefreshFailure {
                id: installer.id().to_string(),
                name: installer.name().to_string(),
                error: e.to_string(),
            }),
        }
    }
    outcome
}

/// `autter install-hooks --refresh-stale`: refresh, record the result for
/// `autter doctor`, and report on stderr. Returns false when an agent failed.
pub fn run_refresh_command() -> bool {
    let binary_path = match crate::mdm::utils::resolve_hook_binary_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("autter: could not locate the autter binary to refresh hooks: {e}");
            return false;
        }
    };
    let params = HookInstallerParams { binary_path };
    let outcome = refresh_stale_hooks(&crate::mdm::agents::get_all_installers(), &params);
    for name in &outcome.refreshed {
        eprintln!("Refreshed autter hooks for {name}; restart it to load them.");
    }
    for failure in &outcome.failed {
        eprintln!(
            "Could not refresh autter hooks for {}: {}. Run `autter install-hooks`.",
            failure.name, failure.error
        );
    }
    let ok = outcome.failed.is_empty();
    record_refresh(outcome.failed);
    ok
}

/// After `autter upgrade` replaced the binary: run the refresh with the newly
/// installed binary (this process is still the old version, and the hooks
/// must match the new one). Its report goes to the terminal.
pub fn refresh_with_installed_binary() {
    let Some(binary) = crate::mdm::utils::stable_install_binary_path() else {
        return;
    };
    let status = std::process::Command::new(&binary)
        .args(["install-hooks", REFRESH_STALE_FLAG])
        .env(WORKER_GUARD_ENV, "1")
        .stdin(std::process::Stdio::null())
        .status();
    if !status.is_ok_and(|s| s.success()) {
        eprintln!(
            "Some agent hooks could not be refreshed for the new version. Run `autter install-hooks`, then `autter doctor`."
        );
    }
}

/// Every autter command found in an agent's hook config files: JSON or TOML
/// string values that run `autter … checkpoint` or `autter … memory-hook`,
/// and argv arrays (Codex's `notify`) joined with spaces.
pub fn autter_commands_in_files(paths: &[PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            collect_json(&value, &mut out);
        } else if let Ok(value) = text.parse::<toml::Table>() {
            collect_toml(&toml::Value::Table(value), &mut out);
        }
    }
    out.dedup();
    out
}

fn is_autter_hook_command(s: &str) -> bool {
    s.contains("autter") && (s.contains("checkpoint") || s.contains("memory-hook"))
}

fn push_argv(items: Vec<&str>, out: &mut Vec<String>) {
    if items
        .iter()
        .any(|s| *s == "checkpoint" || *s == "memory-hook")
        && items.iter().any(|s| s.contains("autter"))
    {
        out.push(items.join(" "));
    }
}

fn collect_json(value: &serde_json::Value, out: &mut Vec<String>) {
    use serde_json::Value;
    match value {
        Value::String(s) if is_autter_hook_command(s) => out.push(s.clone()),
        Value::Array(items) => {
            let strings: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if strings.len() == items.len() && !strings.is_empty() {
                push_argv(strings, out);
            }
            for item in items {
                collect_json(item, out);
            }
        }
        Value::Object(map) => map.values().for_each(|v| collect_json(v, out)),
        _ => {}
    }
}

fn collect_toml(value: &toml::Value, out: &mut Vec<String>) {
    use toml::Value;
    match value {
        Value::String(s) if is_autter_hook_command(s) => out.push(s.clone()),
        Value::Array(items) => {
            let strings: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if strings.len() == items.len() && !strings.is_empty() {
                push_argv(strings, out);
            }
            for item in items {
                collect_toml(item, out);
            }
        }
        Value::Table(map) => map.values().for_each(|v| collect_toml(v, out)),
        _ => {}
    }
}

/// Why this hook command fails when the agent runs it, or `None` when it
/// would run: the binary it names is missing, or its checkpoint preset is not
/// one this autter knows.
pub fn command_problem(command: &str) -> Option<String> {
    let (binary, rest, is_checkpoint) =
        [" checkpoint ", " memory-hook "].iter().find_map(|sep| {
            command.find(sep).map(|at| {
                (
                    &command[..at],
                    &command[at + sep.len()..],
                    *sep == " checkpoint ",
                )
            })
        })?;
    let binary = binary.trim().trim_matches(['"', '\'']);
    if binary.is_empty() {
        return None;
    }
    let looks_like_path = binary.contains('/') || binary.contains('\\');
    if looks_like_path {
        if !Path::new(binary).is_file() {
            return Some(format!("it runs {binary}, which no longer exists"));
        }
    } else if !crate::mdm::utils::binary_exists(binary) {
        return Some(format!("it runs `{binary}`, which is not on PATH"));
    }
    if is_checkpoint {
        let preset = rest.split_whitespace().next().unwrap_or("");
        if !preset.is_empty()
            && !preset.starts_with('-')
            && crate::commands::checkpoint_agent::presets::resolve_preset(preset).is_err()
        {
            return Some(format!(
                "it uses checkpoint preset '{preset}', which this autter does not know"
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdm::hook_installer::HookCheckResult;
    use std::sync::{Arc, Mutex};

    #[test]
    fn missing_binary_breaks_the_hook() {
        let problem =
            command_problem("/no/such/dir/autter checkpoint claude --hook-input stdin").unwrap();
        assert!(problem.contains("/no/such/dir/autter"), "{problem}");
        assert!(problem.contains("no longer exists"), "{problem}");
    }

    #[test]
    fn unknown_preset_breaks_the_hook() {
        let exe = std::env::current_exe().unwrap();
        let cmd = format!(
            "{} checkpoint claude-code --hook-input stdin",
            exe.display()
        );
        let problem = command_problem(&cmd).unwrap();
        assert!(problem.contains("'claude-code'"), "{problem}");
    }

    #[test]
    fn current_binary_and_known_preset_is_cosmetic() {
        let exe = std::env::current_exe().unwrap();
        for preset in ["claude", "antigravity-pre", "codex"] {
            let cmd = format!("{} checkpoint {preset} --hook-input stdin", exe.display());
            assert_eq!(command_problem(&cmd), None, "{cmd}");
        }
        let memory = format!("{} memory-hook claude --hook-input stdin", exe.display());
        assert_eq!(command_problem(&memory), None);
    }

    #[test]
    fn finds_commands_in_json_and_codex_toml() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("hooks.json");
        std::fs::write(
            &json,
            r#"{"autter": {"PreToolUse": [{"hooks": [{"command": "/a/autter checkpoint antigravity-pre --hook-input stdin"}]}]},
                "mine": {"PostToolUse": [{"hooks": [{"command": "./lint.sh"}]}]}}"#,
        )
        .unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(
            &toml,
            "notify = [\"/b/autter\", \"checkpoint\", \"codex\", \"--hook-input\"]\nmodel = \"x\"\n",
        )
        .unwrap();
        let found = autter_commands_in_files(&[json, toml, dir.path().join("missing.json")]);
        assert_eq!(
            found,
            vec![
                "/a/autter checkpoint antigravity-pre --hook-input stdin".to_string(),
                "/b/autter checkpoint codex --hook-input".to_string(),
            ]
        );
    }

    /// Installer double: records whether `install_hooks` ran.
    struct FakeInstaller {
        id: &'static str,
        check: (bool, bool, bool),
        fail: bool,
        installed: Arc<Mutex<bool>>,
    }

    impl FakeInstaller {
        fn new(id: &'static str, check: (bool, bool, bool)) -> Self {
            Self {
                id,
                check,
                fail: false,
                installed: Arc::new(Mutex::new(false)),
            }
        }
    }

    impl HookInstaller for FakeInstaller {
        fn name(&self) -> &str {
            self.id
        }
        fn id(&self) -> &str {
            self.id
        }
        fn check_hooks(&self, _: &HookInstallerParams) -> Result<HookCheckResult, AutterError> {
            Ok(HookCheckResult {
                tool_installed: self.check.0,
                hooks_installed: self.check.1,
                hooks_up_to_date: self.check.2,
            })
        }
        fn install_hooks(
            &self,
            _: &HookInstallerParams,
            _: bool,
        ) -> Result<Option<String>, AutterError> {
            if self.fail {
                return Err(AutterError::Generic("permission denied".into()));
            }
            *self.installed.lock().unwrap() = true;
            Ok(Some(String::new()))
        }
        fn uninstall_hooks(
            &self,
            _: &HookInstallerParams,
            _: bool,
        ) -> Result<Option<String>, AutterError> {
            Ok(None)
        }
    }

    #[test]
    fn refresh_touches_only_stale_consented_agents() {
        let mut failing = FakeInstaller::new("broken", (true, true, false));
        failing.fail = true;
        let fakes = vec![
            FakeInstaller::new("stale", (true, true, false)),
            FakeInstaller::new("current", (true, true, true)),
            FakeInstaller::new("declined", (true, false, false)),
            FakeInstaller::new("absent", (false, false, false)),
            FakeInstaller::new("vscode", (true, true, false)),
            failing,
        ];
        let flags: Vec<(&str, Arc<Mutex<bool>>)> =
            fakes.iter().map(|f| (f.id, f.installed.clone())).collect();
        let installers: Vec<Box<dyn HookInstaller>> = fakes
            .into_iter()
            .map(|f| Box::new(f) as Box<dyn HookInstaller>)
            .collect();
        let params = HookInstallerParams {
            binary_path: PathBuf::from("/usr/local/bin/autter"),
        };
        let outcome = refresh_stale_hooks(&installers, &params);
        assert_eq!(outcome.refreshed, vec!["stale".to_string()]);
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].id, "broken");
        assert!(outcome.failed[0].error.contains("permission denied"));

        let installed: Vec<&str> = flags
            .iter()
            .filter(|(_, flag)| *flag.lock().unwrap())
            .map(|(id, _)| *id)
            .collect();
        // Declined, absent and current agents, and the VS Code extension, are
        // never written to.
        assert_eq!(installed, vec!["stale"]);
    }

    #[test]
    fn refresh_skip_list() {
        assert!(command_skips_refresh(Some("install-hooks")));
        assert!(command_skips_refresh(Some("--version")));
        assert!(command_skips_refresh(None));
        assert!(!command_skips_refresh(Some("checkpoint")));
        assert!(!command_skips_refresh(Some("status")));
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn refresh_skip_list_reads_past_leading_global_flags() {
        for args in [
            &["uninstall-hooks"][..],
            &["--json", "uninstall-hooks"],
            &["--quiet", "--json", "install-hooks"],
            &["-C", "/tmp", "uninstall"],
            &["--no-color", "upgrade"],
            &["--strict", "--json", "onboard"],
            &["--json", "--version"],
            &["--json"],
            &["--help", "status"],
            &["-h"],
            // `-C` with no path: nothing to run.
            &["-C"],
            &[],
        ] {
            assert!(args_skip_refresh(&argv(args)), "{args:?} must not refresh");
        }
        for args in [
            &["status"][..],
            &["--json", "status"],
            &["-C", "/tmp", "--json", "stats"],
            &["checkpoint", "claude", "--hook-input", "stdin"],
            // A flag after the subcommand belongs to the subcommand.
            &["status", "--json"],
            // A path that happens to be named like a skipped command.
            &["-C", "uninstall-hooks", "status"],
        ] {
            assert!(!args_skip_refresh(&argv(args)), "{args:?} must refresh");
        }
    }
}
