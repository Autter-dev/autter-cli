use crate::auth::{AuthState, collect_auth_status, format_unix_timestamp};
use crate::config;
use crate::diagnostics::{DiagnosticCheckResult, GitDiagnosticTarget};
use crate::git::find_repository_in_path;
use crate::git::repository::{
    GitAuthorIdentity, GitConfigIdentityResolution, GitIdentityResolution,
    global_git_config_identity_resolution,
};
use crate::process_timeout::{TimedCommandOutput, run_command_with_timeout};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Duration;

const MIN_GIT_VERSION: GitVersion = GitVersion {
    major: 2,
    minor: 22,
    patch: 0,
};
const MIN_GIT_VERSION_DISPLAY: &str = "2.22.0";
const DEBUG_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const DEBUG_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SKIP_TRACE2_CHECKS_FLAG: &str = "--skip-trace2-checks";

#[derive(Debug, Clone, Copy, Default)]
struct DebugOptions {
    skip_trace2_checks: bool,
}

pub fn handle_debug(args: &[String]) {
    if args
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "help")
    {
        print_debug_help();
        std::process::exit(0);
    }

    let options = match parse_debug_options(args) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("Error: {}", err);
            print_debug_help();
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    };

    let report = build_debug_report(options);
    println!("{}", report);
}

fn print_debug_help() {
    eprintln!("autter debug - Print diagnostic information for troubleshooting");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  autter debug [--skip-trace2-checks]");
    eprintln!("  autter debug --help");
    eprintln!();
    eprintln!("Options:");
    eprintln!(
        "  {}  Skip per-git Trace2 config and Trace2 file self-checks",
        SKIP_TRACE2_CHECKS_FLAG
    );
}

fn parse_debug_options(args: &[String]) -> Result<DebugOptions, String> {
    let mut options = DebugOptions::default();
    for arg in args {
        match arg.as_str() {
            SKIP_TRACE2_CHECKS_FLAG => options.skip_trace2_checks = true,
            unknown => return Err(format!("unknown debug argument: {}", unknown)),
        }
    }
    Ok(options)
}

fn debug_progress(message: impl AsRef<str>) {
    eprintln!(
        "[{}] autter debug: {}",
        chrono::Utc::now().to_rfc3339(),
        message.as_ref()
    );
}

fn build_debug_report(options: DebugOptions) -> String {
    debug_progress("starting debug report");
    let config = config::Config::get();
    let git_cmd = config.git_cmd().to_string();
    debug_progress("resolving configured and shell git paths");
    let git_cmd_realpath = realpath_for_display(&git_cmd);
    let shell_git_lookup = collect_shell_git_lookup();
    debug_progress("checking daemon readiness");
    let daemon_diagnostics = crate::diagnostics::prepare_daemon_for_debug_self_checks(&git_cmd);
    debug_progress(format!(
        "daemon readiness check {}",
        daemon_diagnostics.status.as_str()
    ));
    debug_progress("running git self-checks");
    let git_diagnostics = collect_git_diagnostics(&git_cmd, options);
    debug_progress("collecting system and configuration details");
    debug_progress("checking git versions");
    let git_version = run_command_capture(&git_cmd, &["--version"]);
    let shell_git_version = run_command_capture("git", &["--version"]);
    debug_progress("collecting git config");
    let git_config = collect_git_config_dump(&git_cmd);
    debug_progress("collecting autter config and login state");
    let autter_config = collect_autter_config_dump();
    let platform_info = collect_platform_info();
    let hardware_info = collect_hardware_info();
    let repository_info = collect_repository_info();
    debug_progress("checking AI agent hook status");
    let agent_capture_lines = collect_agent_capture_info();
    let git_committer_identity = collect_git_committer_identity_info(&repository_info);
    let auth_info = collect_auth_status();
    let git_environment = collect_git_environment();
    debug_progress("debug report ready");

    let mut out = String::new();
    let _ = writeln!(out, "autter debug report");
    let _ = writeln!(out, "Generated (UTC): {}", chrono::Utc::now().to_rfc3339());
    let _ = writeln!(out);

    let _ = writeln!(out, "== Versions ==");
    let _ = writeln!(
        out,
        "Autter version: {}",
        if cfg!(debug_assertions) {
            format!("{} (debug)", env!("CARGO_PKG_VERSION"))
        } else {
            env!("CARGO_PKG_VERSION").to_string()
        }
    );
    let _ = writeln!(
        out,
        "Autter binary: {}",
        env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("<unavailable: {}>", e))
    );
    let _ = writeln!(out, "Git binary path: {}", git_cmd);
    let _ = writeln!(out, "Git binary realpath: {}", git_cmd_realpath);
    let _ = writeln!(
        out,
        "Shell git lookup command: {}",
        shell_git_lookup.command
    );
    match shell_git_lookup.path {
        Ok(ref path) => {
            let _ = writeln!(out, "Shell git path: {}", path);
            let _ = writeln!(out, "Shell git realpath: {}", realpath_for_display(path));
        }
        Err(ref err) => {
            let _ = writeln!(out, "Shell git path: <error: {}>", err);
            let _ = writeln!(out, "Shell git realpath: <unavailable>");
        }
    }
    match &git_version {
        Ok(version) => {
            let _ = writeln!(out, "Git version: {}", version);
            append_git_version_check(&mut out, "Git version check", version);
        }
        Err(err) => {
            let _ = writeln!(out, "Git version: <error: {}>", err);
            let _ = writeln!(
                out,
                "Git version check: <error: unable to verify minimum version {}>",
                MIN_GIT_VERSION_DISPLAY
            );
        }
    }
    match &shell_git_version {
        Ok(version) => {
            let _ = writeln!(out, "Shell git version: {}", version);
            append_git_version_check(&mut out, "Shell git version check", version);
        }
        Err(err) => {
            let _ = writeln!(out, "Shell git version: <error: {}>", err);
            let _ = writeln!(
                out,
                "Shell git version check: <error: unable to verify minimum version {}>",
                MIN_GIT_VERSION_DISPLAY
            );
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "== Platform ==");
    let _ = writeln!(out, "OS family: {}", env::consts::FAMILY);
    let _ = writeln!(out, "OS: {}", env::consts::OS);
    let _ = writeln!(out, "Arch: {}", env::consts::ARCH);
    if let Some(kernel) = platform_info.kernel {
        let _ = writeln!(out, "Kernel: {}", kernel);
    } else {
        let _ = writeln!(out, "Kernel: <unavailable>");
    }
    if let Some(hostname) = platform_info.hostname {
        let _ = writeln!(out, "Hostname: {}", hostname);
    } else {
        let _ = writeln!(out, "Hostname: <unavailable>");
    }
    let _ = writeln!(
        out,
        "Shell: {}",
        env::var("SHELL")
            .or_else(|_| env::var("ComSpec"))
            .unwrap_or_else(|_| "<unavailable>".to_string())
    );
    let _ = writeln!(
        out,
        "Current dir: {}",
        env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("<unavailable: {}>", e))
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "== Hardware ==");
    match hardware_info.cpu_model {
        Some(cpu) => {
            let _ = writeln!(out, "CPU: {}", cpu);
        }
        None => {
            let _ = writeln!(out, "CPU: <unavailable>");
        }
    }
    match hardware_info.physical_cores {
        Some(cores) => {
            let _ = writeln!(out, "Physical cores: {}", cores);
        }
        None => {
            let _ = writeln!(out, "Physical cores: <unavailable>");
        }
    }
    match hardware_info.logical_cores {
        Some(cores) => {
            let _ = writeln!(out, "Logical cores: {}", cores);
        }
        None => {
            let _ = writeln!(out, "Logical cores: <unavailable>");
        }
    }
    match hardware_info.total_memory_bytes {
        Some(bytes) => {
            let _ = writeln!(out, "Memory: {}", format_bytes(bytes));
        }
        None => {
            let _ = writeln!(out, "Memory: <unavailable>");
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "== Repository ==");
    let _ = writeln!(out, "In repository: {}", repository_info.in_repository);
    if let Some(err) = repository_info.error {
        let _ = writeln!(out, "Repository detection: {}", err);
    } else {
        if let Some(workdir) = repository_info.workdir {
            let _ = writeln!(out, "Workdir: {}", workdir);
        }
        if let Some(git_dir) = repository_info.git_dir {
            let _ = writeln!(out, "Git dir: {}", git_dir);
        }
        if let Some(common_dir) = repository_info.common_dir {
            let _ = writeln!(out, "Git common dir: {}", common_dir);
        }
        if let Some(branch) = repository_info.branch {
            let _ = writeln!(out, "Branch: {}", branch);
        }
        if let Some(head) = repository_info.head {
            let _ = writeln!(out, "HEAD: {}", head);
        }
        if let Some(hooks_path) = repository_info.hooks_path {
            let _ = writeln!(out, "core.hooksPath: {}", hooks_path);
        }
        if !repository_info.remotes.is_empty() {
            let _ = writeln!(out, "Remotes:");
            for (name, url) in repository_info.remotes {
                let _ = writeln!(out, "  {} = {}", name, url);
            }
        }
    }
    let _ = writeln!(out);

    append_git_committer_identity(&mut out, &git_committer_identity);
    let _ = writeln!(out);

    append_git_diagnostics(&mut out, &daemon_diagnostics, &git_diagnostics);
    let _ = writeln!(out);

    let _ = writeln!(out, "== AI Agent Capture ==");
    for line in &agent_capture_lines {
        let _ = writeln!(out, "{}", line);
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "== Git Config ==");
    let _ = writeln!(out, "Command: {}", git_config.command);
    match git_config.output {
        Ok(config_output) => {
            append_indented_block(&mut out, &config_output);
        }
        Err(err) => {
            let _ = writeln!(out, "  <error: {}>", err);
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "== Autter Config ==");
    match autter_config {
        Ok(config_output) => {
            append_indented_block(&mut out, &config_output);
        }
        Err(err) => {
            let _ = writeln!(out, "  <error: {}>", err);
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "== Autter Login ==");
    let _ = writeln!(out, "Credential backend: {}", auth_info.backend);
    match &auth_info.state {
        AuthState::LoggedOut => {
            let _ = writeln!(out, "Status: logged out");
        }
        AuthState::LoggedIn => {
            let _ = writeln!(out, "Status: logged in");
        }
        AuthState::RefreshExpired => {
            let _ = writeln!(out, "Status: credentials expired (refresh token expired)");
            let _ = writeln!(out, "  fix: run `autter login` to sign in again");
        }
        AuthState::Error(err) => {
            let _ = writeln!(out, "Status: <error: {}>", err);
            let _ = writeln!(
                out,
                "  fix: run `autter login` to refresh stored credentials"
            );
        }
    }
    if let Some(expires_at) = auth_info.access_token_expires_at {
        let _ = writeln!(
            out,
            "Access token expires at: {}",
            format_unix_timestamp(expires_at)
        );
    }
    if let Some(expires_at) = auth_info.refresh_token_expires_at {
        let _ = writeln!(
            out,
            "Refresh token expires at: {}",
            format_unix_timestamp(expires_at)
        );
    }
    if let Some(user_id) = auth_info.user_id {
        let _ = writeln!(out, "User ID: {}", user_id);
    } else if matches!(
        &auth_info.state,
        AuthState::LoggedIn | AuthState::RefreshExpired
    ) {
        let _ = writeln!(out, "User ID: <unavailable>");
    }
    if let Some(email) = auth_info.email {
        let _ = writeln!(out, "Email: {}", email);
    } else if matches!(
        &auth_info.state,
        AuthState::LoggedIn | AuthState::RefreshExpired
    ) {
        let _ = writeln!(out, "Email: <unavailable>");
    }
    if let Some(name) = auth_info.name {
        let _ = writeln!(out, "Name: {}", name);
    } else if matches!(
        &auth_info.state,
        AuthState::LoggedIn | AuthState::RefreshExpired
    ) {
        let _ = writeln!(out, "Name: <unavailable>");
    }
    if let Some(personal_org_id) = auth_info.personal_org_id {
        let _ = writeln!(out, "Personal org ID: {}", personal_org_id);
    }
    if !auth_info.orgs.is_empty() {
        let _ = writeln!(out, "Organizations:");
        for org in auth_info.orgs {
            let org_id = org.org_id.unwrap_or_else(|| "<unknown-id>".to_string());
            let org_slug = org.org_slug.unwrap_or_else(|| "<unknown-slug>".to_string());
            let org_name = org.org_name.unwrap_or_else(|| "<unknown-name>".to_string());
            let role = org.role.unwrap_or_else(|| "<unknown-role>".to_string());
            let _ = writeln!(
                out,
                "  - {} ({}) [{}] role={}",
                org_slug, org_name, org_id, role
            );
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "== Git Environment ==");
    if git_environment.is_empty() {
        let _ = writeln!(
            out,
            "No AUTTER_*, AUTTER_*, or GIT_* environment variables are set."
        );
    } else {
        let _ = writeln!(out, "AUTTER_*, AUTTER_*, and GIT_* variables set:");
        for entry in git_environment {
            let _ = writeln!(out, "  {}", entry);
        }
    }

    out
}

/// Per-agent hook status plus the VS Code native-hooks chain. This is the
/// capture-side counterpart to the git-side diagnostics: `install-hooks`
/// only verifies its own writes, so this section answers "would an edit made
/// right now actually checkpoint?" per agent surface.
fn collect_agent_capture_info() -> Vec<String> {
    use crate::mdm::agents::get_all_installers;
    use crate::mdm::hook_installer::HookInstallerParams;

    let mut lines = Vec::new();

    let binary_path = env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("autter"));
    let params = HookInstallerParams { binary_path };

    lines.push("Hooks status by agent (managed by `autter install-hooks`):".to_string());
    for installer in get_all_installers() {
        let name = installer.name();
        match installer.check_hooks(&params) {
            Ok(result) => {
                let status = if !result.tool_installed {
                    "not detected".to_string()
                } else if installer.id() == "vscode" {
                    // VS Code has no config-file hooks; hooks_installed
                    // reflects the autter extension, which carries both
                    // manual-edit (known_human) and legacy AI capture.
                    if result.hooks_installed {
                        "detected, autter extension installed".to_string()
                    } else {
                        "detected, autter extension NOT installed — manual edits and (on older VS Code) Copilot edits are not captured".to_string()
                    }
                } else if !result.hooks_installed {
                    "detected, hooks NOT installed — run `autter install-hooks`".to_string()
                } else if result.hooks_up_to_date {
                    "detected, hooks installed (up to date)".to_string()
                } else {
                    "detected, hooks installed (update available — run `autter install-hooks`)"
                        .to_string()
                };
                lines.push(format!("  {}: {}", name, status));
            }
            Err(err) => {
                lines.push(format!("  {}: <error: {}>", name, err));
            }
        }
    }
    lines.push(String::new());

    lines.extend(collect_vscode_native_hooks_chain());
    lines.push(String::new());

    lines.push("Capture boundary (what checkpoints, and what it requires):".to_string());
    lines.push(
        "  - VS Code built-in Copilot agent (agent mode): VS Code >= 1.109.3 fires native agent hooks (Preview) which run autter; older VS Code uses the autter extension's built-in detection. Restart VS Code after `autter install-hooks`."
            .to_string(),
    );
    lines.push("  - GitHub Copilot CLI: captured via ~/.copilot/hooks/autter.json.".to_string());
    lines.push(
        "  - VS Code inline/tab completions: experimental and OFF by default (VS Code setting `autter.experiments.aiTabTracking`)."
            .to_string(),
    );
    lines.push(
        "  - Manual (human) edits: captured on save by the autter editor extensions (VS Code family, JetBrains, Visual Studio). Edits made without an autter extension are committed as untracked."
            .to_string(),
    );
    lines.push(
        "  - Remote dev (SSH/WSL/devcontainers): agents and hooks run on the remote host — install autter and run `autter install-hooks` there too."
            .to_string(),
    );
    lines
}

/// Outcome of walking the VS Code native-hooks chain, shared between the
/// debug report (which prints the lines) and `autter doctor` (which turns the
/// outcome into a pass/warn/fail check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VsCodeChainOutcome {
    /// No VS Code CLI found on this machine.
    NotDetected,
    /// VS Code found but its version could not be determined.
    VersionUnknown,
    /// VS Code predates native agent hooks; the autter extension captures.
    LegacyExtensionMode,
    /// Native-hooks mode and every link in the chain is in place.
    Complete,
    /// Native-hooks mode but the hook file or chat settings are missing.
    Incomplete,
}

pub(crate) struct VsCodeNativeHooksChain {
    pub(crate) outcome: VsCodeChainOutcome,
    pub(crate) lines: Vec<String>,
}

fn collect_vscode_native_hooks_chain() -> Vec<String> {
    inspect_vscode_native_hooks_chain().lines
}

/// Walk the chain VS Code's built-in Copilot agent needs for AI edits to
/// checkpoint on VS Code >= 1.109.3: the autter extension stands down there
/// and capture only happens if VS Code loads ~/.copilot/hooks/autter.json.
pub(crate) fn inspect_vscode_native_hooks_chain() -> VsCodeNativeHooksChain {
    use crate::mdm::utils::{
        MIN_VSCODE_NATIVE_HOOKS_VERSION, VSCODE_USER_COPILOT_HOOKS_LOCATION, get_editor_version,
        home_dir, parse_version_triple, resolve_editor_cli, settings_paths_for_products,
    };

    let mut lines = Vec::new();
    lines.push("VS Code built-in Copilot agent chain:".to_string());

    let Some(cli) = resolve_editor_cli("code") else {
        lines.push("  VS Code CLI not found; skipping chain checks".to_string());
        return VsCodeNativeHooksChain {
            outcome: VsCodeChainOutcome::NotDetected,
            lines,
        };
    };

    let version_str = match get_editor_version(&cli) {
        Ok(v) => v,
        Err(err) => {
            lines.push(format!("  VS Code version: <error: {}>", err));
            return VsCodeNativeHooksChain {
                outcome: VsCodeChainOutcome::VersionUnknown,
                lines,
            };
        }
    };
    let first_line = version_str.lines().next().unwrap_or("").trim().to_string();
    lines.push(format!("  VS Code version: {}", first_line));

    let (min_major, min_minor, min_patch) = MIN_VSCODE_NATIVE_HOOKS_VERSION;
    let native_mode = parse_version_triple(&version_str)
        .map(|v| v >= MIN_VSCODE_NATIVE_HOOKS_VERSION)
        .unwrap_or(false);

    if !native_mode {
        lines.push(format!(
            "  Capture mode: autter extension detection (VS Code predates native agent hooks, added in {}.{}.{})",
            min_major, min_minor, min_patch
        ));
        return VsCodeNativeHooksChain {
            outcome: VsCodeChainOutcome::LegacyExtensionMode,
            lines,
        };
    }

    lines.push(format!(
        "  Capture mode: native agent hooks (VS Code >= {}.{}.{}; the autter extension defers AI-edit capture to VS Code)",
        min_major, min_minor, min_patch
    ));

    let hooks_file = home_dir()
        .join(".copilot")
        .join("hooks")
        .join("autter.json");
    let hooks_file_present = hooks_file.exists();
    lines.push(format!(
        "  Hook file {}: {}",
        hooks_file.display(),
        if hooks_file_present {
            "present"
        } else {
            "MISSING"
        }
    ));

    let mut any_location_enabled = false;
    for settings_path in settings_paths_for_products(&["Code", "Code - Insiders"]) {
        if !settings_path.exists() {
            continue;
        }
        let (use_hooks, copilot_location) = read_chat_hook_settings(&settings_path);
        lines.push(format!("  Settings {}:", settings_path.display()));
        lines.push(format!(
            "    chat.useHooks: {}",
            format_setting_bool(use_hooks)
        ));
        lines.push(format!(
            "    chat.hookFilesLocations[\"{}\"]: {}",
            VSCODE_USER_COPILOT_HOOKS_LOCATION,
            format_setting_bool(copilot_location)
        ));
        if copilot_location == Some(true) {
            any_location_enabled = true;
        }
    }

    if hooks_file_present && any_location_enabled {
        lines.push(
            "  Chain status: OK — Copilot agent-mode edits should checkpoint (restart VS Code if hooks were just installed)"
                .to_string(),
        );
        return VsCodeNativeHooksChain {
            outcome: VsCodeChainOutcome::Complete,
            lines,
        };
    }
    lines.push(
        "  Chain status: INCOMPLETE — Copilot agent-mode edits are likely NOT being captured; run `autter install-hooks` and restart VS Code"
            .to_string(),
    );
    VsCodeNativeHooksChain {
        outcome: VsCodeChainOutcome::Incomplete,
        lines,
    }
}

/// Read `chat.useHooks` and the `~/.copilot/hooks` entry of
/// `chat.hookFilesLocations` from a VS Code settings.json (JSONC tolerated).
fn read_chat_hook_settings(settings_path: &Path) -> (Option<bool>, Option<bool>) {
    use crate::mdm::utils::VSCODE_USER_COPILOT_HOOKS_LOCATION;
    use jsonc_parser::ParseOptions;
    use jsonc_parser::cst::CstRootNode;

    let Ok(content) = fs::read_to_string(settings_path) else {
        return (None, None);
    };
    let parse_input = if content.trim().is_empty() {
        "{}".to_string()
    } else {
        content
    };
    let Ok(root) = CstRootNode::parse(&parse_input, &ParseOptions::default()) else {
        return (None, None);
    };
    let Some(object) = root.object_value() else {
        return (None, None);
    };

    let use_hooks = object
        .get("chat.useHooks")
        .and_then(|prop| prop.value())
        .and_then(|value| value.as_boolean_lit())
        .map(|lit| lit.value());

    let copilot_location = object
        .object_value("chat.hookFilesLocations")
        .and_then(|locations| locations.get(VSCODE_USER_COPILOT_HOOKS_LOCATION))
        .and_then(|prop| prop.value())
        .and_then(|value| value.as_boolean_lit())
        .map(|lit| lit.value());

    (use_hooks, copilot_location)
}

fn format_setting_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "<not set>",
    }
}

struct GitDebugDiagnostics {
    target: GitDiagnosticTarget,
    trace2_config: DiagnosticCheckResult,
    attribution: DiagnosticCheckResult,
    trace2: DiagnosticCheckResult,
}

struct GitCommitterIdentityInfo {
    global_config: Result<GitConfigIdentityResolution, String>,
    repository: RepositoryCommitterIdentity,
    author_config: config::AuthorConfig,
}

enum RepositoryCommitterIdentity {
    InRepository(GitIdentityResolution),
    NotInRepository(String),
}

fn collect_git_committer_identity_info(
    repository_info: &RepositoryInfo,
) -> GitCommitterIdentityInfo {
    let global_config = global_git_config_identity_resolution().map_err(|e| e.to_string());
    let author_config = config::Config::fresh_author_cached();
    let repository = repository_info
        .committer_identity
        .clone()
        .map(RepositoryCommitterIdentity::InRepository)
        .unwrap_or_else(|| {
            RepositoryCommitterIdentity::NotInRepository(
                repository_info
                    .error
                    .clone()
                    .unwrap_or_else(|| "not in repository".to_string()),
            )
        });

    GitCommitterIdentityInfo {
        global_config,
        repository,
        author_config,
    }
}

fn append_git_committer_identity(out: &mut String, identity: &GitCommitterIdentityInfo) {
    let _ = writeln!(out, "== Git Committer Identity ==");
    let _ = writeln!(out, "Global git config identity:");
    match &identity.global_config {
        Ok(global) => {
            append_raw_git_config_identity(out, global, "  ");
            append_git_author_identity(out, &global.identity, "  ");
        }
        Err(err) => {
            let _ = writeln!(out, "  <error: {}>", err);
        }
    }

    let _ = writeln!(out, "Repository effective committer identity:");
    match &identity.repository {
        RepositoryCommitterIdentity::InRepository(resolution) => {
            let raw = resolution
                .raw_git_var
                .as_deref()
                .unwrap_or("<unavailable; autter used config fallback>");
            let _ = writeln!(out, "  Raw GIT_COMMITTER_IDENT: {}", raw);
            append_git_author_identity(out, &resolution.identity, "  ");
        }
        RepositoryCommitterIdentity::NotInRepository(err) => {
            let _ = writeln!(out, "  <not in repository: {}>", err);
        }
    }

    let _ = writeln!(out, "Autter author config override:");
    append_author_config(out, &identity.author_config, "  ");

    let _ = writeln!(out, "Autter effective author identity:");
    match &identity.repository {
        RepositoryCommitterIdentity::InRepository(resolution) => {
            let effective_author = resolution
                .identity
                .with_author_config(&identity.author_config);
            append_git_author_identity(out, &effective_author, "  ");
        }
        RepositoryCommitterIdentity::NotInRepository(err) => {
            let _ = writeln!(out, "  <not in repository: {}>", err);
        }
    }
}

fn append_raw_git_config_identity(
    out: &mut String,
    identity: &GitConfigIdentityResolution,
    prefix: &str,
) {
    let raw_name = identity.raw_name.as_deref().unwrap_or("<unset>");
    let raw_email = identity.raw_email.as_deref().unwrap_or("<unset>");

    let _ = writeln!(out, "{}Raw user.name: {}", prefix, raw_name);
    let _ = writeln!(out, "{}Raw user.email: {}", prefix, raw_email);
}

fn append_git_author_identity(out: &mut String, identity: &GitAuthorIdentity, prefix: &str) {
    let formatted = identity
        .formatted()
        .unwrap_or_else(|| "<unavailable>".to_string());
    let name = identity.name.as_deref().unwrap_or("<unavailable>");
    let email = identity.email.as_deref().unwrap_or("<unavailable>");

    let _ = writeln!(out, "{}Formatted: {}", prefix, formatted);
    let _ = writeln!(out, "{}Parsed name: {}", prefix, name);
    let _ = writeln!(out, "{}Parsed email: {}", prefix, email);
}

fn append_author_config(out: &mut String, author: &config::AuthorConfig, prefix: &str) {
    let name = author.name.as_deref().unwrap_or("<unset>");
    let email = author.email.as_deref().unwrap_or("<unset>");

    let _ = writeln!(out, "{}author.name: {}", prefix, name);
    let _ = writeln!(out, "{}author.email: {}", prefix, email);
}

fn collect_git_diagnostics(
    configured_git: &str,
    options: DebugOptions,
) -> Vec<GitDebugDiagnostics> {
    let targets = vec![
        GitDiagnosticTarget::new("configured git", configured_git),
        GitDiagnosticTarget::new("terminal git", "git"),
    ];

    let trace2_configs: Vec<_> = if options.skip_trace2_checks {
        debug_progress(format!(
            "skipping Trace2 config checks ({})",
            SKIP_TRACE2_CHECKS_FLAG
        ));
        targets
            .iter()
            .map(|_| skipped_trace2_check("trace2 global config check skipped"))
            .collect()
    } else {
        targets
            .iter()
            .map(|target| {
                debug_progress(format!("checking Trace2 config for {}", target.label));
                let result = crate::diagnostics::check_trace2_global_config(target);
                debug_progress(format!(
                    "Trace2 config check for {} {}",
                    target.label,
                    result.status.as_str()
                ));
                result
            })
            .collect()
    };
    let attribution_handles: Vec<_> = targets
        .clone()
        .into_iter()
        .map(|target| {
            let label = target.label.clone();
            debug_progress(format!("starting attribution self-check for {}", label));
            std::thread::spawn(move || {
                let result = crate::diagnostics::run_attribution_self_check(&target);
                debug_progress(format!(
                    "attribution self-check for {} {}",
                    label,
                    result.status.as_str()
                ));
                result
            })
        })
        .collect();
    let attributions: Vec<_> = attribution_handles
        .into_iter()
        .map(|handle| {
            handle.join().unwrap_or_else(|_| {
                DiagnosticCheckResult::failed(
                    "attribution self-check failed",
                    vec!["attribution self-check worker panicked".to_string()],
                    Vec::new(),
                )
            })
        })
        .collect();
    // Trace2 file checks temporarily rewrite global git config, so they must remain serialized.
    let trace2_checks: Vec<_> = if options.skip_trace2_checks {
        debug_progress(format!(
            "skipping Trace2 file self-checks ({})",
            SKIP_TRACE2_CHECKS_FLAG
        ));
        targets
            .iter()
            .map(|_| skipped_trace2_check("trace2 file self-check skipped"))
            .collect()
    } else {
        targets
            .iter()
            .map(|target| {
                debug_progress(format!(
                    "starting Trace2 file self-check for {}",
                    target.label
                ));
                let result = crate::diagnostics::run_trace2_file_self_check(target);
                debug_progress(format!(
                    "Trace2 file self-check for {} {}",
                    target.label,
                    result.status.as_str()
                ));
                result
            })
            .collect()
    };

    targets
        .into_iter()
        .zip(trace2_configs)
        .zip(attributions)
        .zip(trace2_checks)
        .map(
            |(((target, trace2_config), attribution), trace2)| GitDebugDiagnostics {
                target,
                trace2_config,
                attribution,
                trace2,
            },
        )
        .collect()
}

fn skipped_trace2_check(summary: &str) -> DiagnosticCheckResult {
    DiagnosticCheckResult::skipped(
        summary,
        vec![format!("skipped by {}", SKIP_TRACE2_CHECKS_FLAG)],
    )
}

fn append_git_diagnostics(
    out: &mut String,
    daemon: &DiagnosticCheckResult,
    diagnostics: &[GitDebugDiagnostics],
) {
    let _ = writeln!(out, "== Git Self Checks ==");
    let _ = writeln!(out, "daemon");
    append_diagnostic_check(out, "Daemon check", daemon, false);
    for diagnostic in diagnostics {
        let _ = writeln!(
            out,
            "{} (program: {})",
            diagnostic.target.label, diagnostic.target.program
        );
        append_diagnostic_check(out, "Trace2 config check", &diagnostic.trace2_config, false);
        append_diagnostic_check(
            out,
            "Attribution self-check",
            &diagnostic.attribution,
            false,
        );
        append_diagnostic_check(out, "Trace2 file self-check", &diagnostic.trace2, true);
    }
}

fn append_diagnostic_check(
    out: &mut String,
    label: &str,
    check: &DiagnosticCheckResult,
    always_show_trace2: bool,
) {
    let _ = writeln!(
        out,
        "  {}: {} - {}",
        label,
        check.status.as_str(),
        check.summary
    );
    if check.status == crate::diagnostics::DiagnosticStatus::Failed
        && let Some(remediation) = &check.remediation
    {
        let _ = writeln!(out, "    fix: {}", remediation);
    }
    for detail in &check.details {
        let _ = writeln!(out, "    {}", detail);
    }

    if always_show_trace2 && let Some(trace2_json) = check.trace2_json.as_ref() {
        let _ = writeln!(out, "    trace2 JSON received:");
        append_indented_block_with_prefix(out, trace2_json, "      ");
    }

    if check.status == crate::diagnostics::DiagnosticStatus::Failed {
        let _ = writeln!(out, "    command log:");
        for command in &check.commands {
            let _ = writeln!(out, "      $ {}", command.command);
            if let Some(cwd) = &command.cwd {
                let _ = writeln!(out, "        cwd: {}", cwd);
            }
            let _ = writeln!(
                out,
                "        status: {}",
                if command.timed_out {
                    "<timeout>".to_string()
                } else {
                    command
                        .status
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "<unavailable>".to_string())
                }
            );
            if command.timed_out {
                let _ = writeln!(out, "        timed out: yes");
            }
            if !command.stdout.trim().is_empty() {
                let _ = writeln!(out, "        stdout:");
                append_indented_block_with_prefix(out, &command.stdout, "          ");
            }
            if !command.stderr.trim().is_empty() {
                let _ = writeln!(out, "        stderr:");
                append_indented_block_with_prefix(out, &command.stderr, "          ");
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct GitVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl std::fmt::Display for GitVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn append_git_version_check(out: &mut String, label: &str, version_output: &str) {
    match parse_git_version(version_output) {
        Some(version) if version >= MIN_GIT_VERSION => {
            let _ = writeln!(
                out,
                "{}: version meets or exceeds minimum version of {}",
                label, MIN_GIT_VERSION_DISPLAY
            );
        }
        Some(version) => {
            let _ = writeln!(
                out,
                "{}: ERROR: detected Git version {} is below minimum version {}",
                label, version, MIN_GIT_VERSION_DISPLAY
            );
            let _ = writeln!(
                out,
                "  fix: upgrade git to {} or newer, then re-run `autter debug`",
                MIN_GIT_VERSION_DISPLAY
            );
        }
        None => {
            let _ = writeln!(
                out,
                "{}: <error: could not parse Git version from '{}'; minimum version is {}>",
                label, version_output, MIN_GIT_VERSION_DISPLAY
            );
        }
    }
}

fn parse_git_version(output: &str) -> Option<GitVersion> {
    output.split_whitespace().find_map(parse_git_version_token)
}

fn parse_git_version_token(token: &str) -> Option<GitVersion> {
    let token = token.trim_start_matches('v');
    let mut parts = token.split('.');
    let major = parse_leading_u32(parts.next()?)?;
    let minor = parse_leading_u32(parts.next()?)?;
    let patch = parts.next().map(parse_leading_u32).unwrap_or(Some(0))?;

    Some(GitVersion {
        major,
        minor,
        patch,
    })
}

fn parse_leading_u32(value: &str) -> Option<u32> {
    let digits = value
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

struct ShellGitLookup {
    command: String,
    path: Result<String, String>,
}

fn collect_shell_git_lookup() -> ShellGitLookup {
    #[cfg(windows)]
    {
        collect_windows_shell_git_lookup()
    }

    #[cfg(not(windows))]
    {
        collect_unix_shell_git_lookup()
    }
}

#[cfg(not(windows))]
fn collect_unix_shell_git_lookup() -> ShellGitLookup {
    let shell = env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "sh".to_string());
    let command = format!("{} -lc 'which git'", shell);
    let path = run_command_capture(&shell, &["-lc", "which git"])
        .and_then(|output| select_lookup_path(&output));

    ShellGitLookup { command, path }
}

#[cfg(windows)]
fn collect_windows_shell_git_lookup() -> ShellGitLookup {
    let comspec = env::var("ComSpec")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "cmd.exe".to_string());
    let command = format!("{} /C where git", comspec);
    let path = run_command_capture(&comspec, &["/C", "where git"])
        .and_then(|output| select_lookup_path(&output));

    ShellGitLookup { command, path }
}

fn select_lookup_path(output: &str) -> Result<String, String> {
    let mut first_non_empty = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if first_non_empty.is_none() {
            first_non_empty = Some(trimmed.to_string());
        }

        if Path::new(trimmed).exists() {
            return Ok(trimmed.to_string());
        }
    }

    first_non_empty.ok_or_else(|| "empty output".to_string())
}

fn realpath_for_display(path: &str) -> String {
    fs::canonicalize(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<error: {}>", e))
}

fn append_indented_block(out: &mut String, content: &str) {
    if content.trim().is_empty() {
        let _ = writeln!(out, "  <empty>");
        return;
    }
    for line in content.lines() {
        let _ = writeln!(out, "  {}", line);
    }
}

fn append_indented_block_with_prefix(out: &mut String, content: &str, prefix: &str) {
    if content.trim().is_empty() {
        let _ = writeln!(out, "{}<empty>", prefix);
        return;
    }
    for line in content.lines() {
        let _ = writeln!(out, "{}{}", prefix, line);
    }
}

fn run_command_capture(program: &str, args: &[&str]) -> Result<String, String> {
    run_command_capture_with_timeout(program, args, DEBUG_COMMAND_TIMEOUT)
}

fn run_command_capture_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    let command = format_command_for_error(program, args);
    let output = run_command_with_timeout(
        program,
        args,
        None,
        timeout,
        DEBUG_COMMAND_POLL_INTERVAL,
        &[],
    )
    .map_err(|e| {
        format!(
            "failed to execute '{}': {}",
            program,
            strip_execute_prefix(&e)
        )
    })?;

    if output.timed_out {
        return Err(format_timeout_capture_error(&command, timeout, output));
    }
    if output.wait_error.is_some() {
        return Err(format_wait_capture_error(&command, output));
    }

    command_output_to_result(output)
}

fn command_output_to_result(output: TimedCommandOutput) -> Result<String, String> {
    if output.status != Some(0) {
        let mut stderr = output.stderr.trim().to_string();
        append_debug_diagnostics(&mut stderr, &output.diagnostics);
        let code = output
            .status
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        if stderr.is_empty() {
            return Err(format!("exit code {}", code));
        }
        return Err(format!("exit code {}: {}", code, stderr));
    }

    Ok(output.stdout)
}

fn format_timeout_capture_error(
    command: &str,
    timeout: Duration,
    output: TimedCommandOutput,
) -> String {
    let mut message = format!(
        "timed out after {:.1}s running '{}'",
        timeout.as_secs_f64(),
        command
    );
    append_debug_diagnostics(&mut message, &output.diagnostics);
    if let Some(wait_error) = output.wait_error {
        message.push_str(&format!("; failed while waiting: {}", wait_error));
    }
    if !output.stdout.trim().is_empty() {
        message.push_str(&format!(
            "; stdout before timeout: {}",
            output.stdout.trim()
        ));
    }
    if !output.stderr.trim().is_empty() {
        message.push_str(&format!(
            "; stderr before timeout: {}",
            output.stderr.trim()
        ));
    }
    message
}

fn format_wait_capture_error(command: &str, output: TimedCommandOutput) -> String {
    let wait_error = output.wait_error.as_deref().unwrap_or("unknown wait error");
    let mut message = format!("failed while waiting for '{}': {}", command, wait_error);
    append_debug_diagnostics(&mut message, &output.diagnostics);
    if !output.stdout.trim().is_empty() {
        message.push_str(&format!(
            "; stdout before wait failure: {}",
            output.stdout.trim()
        ));
    }
    if !output.stderr.trim().is_empty() {
        message.push_str(&format!(
            "; stderr before wait failure: {}",
            output.stderr.trim()
        ));
    }
    message
}

fn append_debug_diagnostics(message: &mut String, diagnostics: &[String]) {
    for diagnostic in diagnostics {
        if !message.is_empty() {
            message.push_str("; ");
        }
        message.push_str(diagnostic);
    }
}

fn strip_execute_prefix(error: &str) -> &str {
    error.strip_prefix("failed to execute: ").unwrap_or(error)
}

fn format_command_for_error(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .map(shell_quote_for_error)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote_for_error(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=@".contains(ch))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[derive(Default)]
struct PlatformInfo {
    kernel: Option<String>,
    hostname: Option<String>,
}

fn collect_platform_info() -> PlatformInfo {
    PlatformInfo {
        kernel: collect_kernel_details(),
        hostname: collect_hostname(),
    }
}

fn collect_kernel_details() -> Option<String> {
    #[cfg(unix)]
    {
        run_command_capture("uname", &["-srm"]).ok()
    }
    #[cfg(windows)]
    {
        run_command_capture("cmd", &["/C", "ver"]).ok()
    }
}

fn collect_hostname() -> Option<String> {
    if let Ok(hostname) = env::var("HOSTNAME")
        && !hostname.trim().is_empty()
    {
        return Some(hostname);
    }

    if let Ok(hostname) = env::var("COMPUTERNAME")
        && !hostname.trim().is_empty()
    {
        return Some(hostname);
    }

    run_command_capture("hostname", &[]).ok()
}

#[derive(Default)]
struct HardwareInfo {
    cpu_model: Option<String>,
    physical_cores: Option<usize>,
    logical_cores: Option<usize>,
    total_memory_bytes: Option<u64>,
}

fn collect_hardware_info() -> HardwareInfo {
    let mut info = HardwareInfo {
        logical_cores: std::thread::available_parallelism()
            .ok()
            .map(std::num::NonZeroUsize::get),
        ..HardwareInfo::default()
    };

    #[cfg(target_os = "macos")]
    {
        info.cpu_model = run_command_capture("sysctl", &["-n", "machdep.cpu.brand_string"]).ok();
        info.physical_cores = run_command_capture("sysctl", &["-n", "hw.physicalcpu"])
            .ok()
            .and_then(|s| s.parse::<usize>().ok());
        info.logical_cores = run_command_capture("sysctl", &["-n", "hw.logicalcpu"])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .or(info.logical_cores);
        info.total_memory_bytes = run_command_capture("sysctl", &["-n", "hw.memsize"])
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
    }

    #[cfg(target_os = "linux")]
    {
        info.cpu_model = read_linux_cpu_model();
        info.total_memory_bytes = read_linux_total_memory();
    }

    #[cfg(windows)]
    {
        info.cpu_model = run_command_capture(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "(Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name)",
            ],
        )
        .ok()
        .filter(|s| !s.trim().is_empty());

        info.physical_cores = run_command_capture(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "(Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty NumberOfCores)",
            ],
        )
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok());

        info.total_memory_bytes = run_command_capture(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "(Get-CimInstance Win32_ComputerSystem | Select-Object -ExpandProperty TotalPhysicalMemory)",
            ],
        )
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    }

    info
}

#[cfg(target_os = "linux")]
fn read_linux_cpu_model() -> Option<String> {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in cpuinfo.lines() {
        if let Some((_, value)) = line.split_once(':')
            && line.starts_with("model name")
        {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn read_linux_total_memory() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{:.2} {} ({} bytes)", value, UNITS[unit], bytes)
}

struct RepositoryInfo {
    in_repository: bool,
    error: Option<String>,
    workdir: Option<String>,
    git_dir: Option<String>,
    common_dir: Option<String>,
    branch: Option<String>,
    head: Option<String>,
    hooks_path: Option<String>,
    remotes: Vec<(String, String)>,
    committer_identity: Option<GitIdentityResolution>,
}

fn collect_repository_info() -> RepositoryInfo {
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    let repo = match find_repository_in_path(&cwd) {
        Ok(repo) => repo,
        Err(e) => {
            return RepositoryInfo {
                in_repository: false,
                error: Some(e.to_string()),
                workdir: None,
                git_dir: None,
                common_dir: None,
                branch: None,
                head: None,
                hooks_path: None,
                remotes: Vec::new(),
                committer_identity: None,
            };
        }
    };

    let head = repo.head().ok();
    let committer_identity = repo.git_author_identity_resolution();

    RepositoryInfo {
        in_repository: true,
        error: None,
        workdir: repo.workdir().ok().map(|p| p.display().to_string()),
        git_dir: Some(repo.path().display().to_string()),
        common_dir: Some(repo.common_dir().display().to_string()),
        branch: head.as_ref().and_then(|h| h.shorthand().ok()),
        head: head.as_ref().and_then(|h| h.target().ok()),
        hooks_path: repo.config_get_str("core.hooksPath").ok().flatten(),
        remotes: repo.remotes_with_urls().unwrap_or_default(),
        committer_identity: Some(committer_identity),
    }
}

struct GitConfigDump {
    command: String,
    output: Result<String, String>,
}

fn collect_git_config_dump(git_cmd: &str) -> GitConfigDump {
    let attempts: &[&[&str]] = &[
        &["config", "--list", "--show-origin", "--show-scope"],
        &["config", "--list", "--show-origin"],
        &["config", "--list"],
    ];

    let mut last_error = String::new();
    for args in attempts {
        match run_command_capture(git_cmd, args) {
            Ok(output) => {
                let redacted = output
                    .lines()
                    .map(redact_git_config_line)
                    .collect::<Vec<_>>()
                    .join("\n");
                return GitConfigDump {
                    command: format!("{} {}", git_cmd, args.join(" ")),
                    output: Ok(redacted),
                };
            }
            Err(err) => {
                last_error = err;
            }
        }
    }

    GitConfigDump {
        command: format!("{} config --list --show-origin --show-scope", git_cmd),
        output: Err(last_error),
    }
}

fn redact_git_config_line(line: &str) -> String {
    if !line.contains('\t') {
        if let Some((key, value)) = line.split_once('=')
            && should_redact_key_value(key, value)
        {
            return format!("{}=[REDACTED]", key);
        }
        return line.to_string();
    }

    let mut parts = line.splitn(3, '\t');
    let first = match parts.next() {
        Some(v) => v,
        None => return line.to_string(),
    };
    let second = match parts.next() {
        Some(v) => v,
        None => return line.to_string(),
    };

    match parts.next() {
        // 3-field format: scope \t origin \t key=value
        // (from `git config --list --show-origin --show-scope`)
        Some(key_value) => {
            let (key, value) = match key_value.split_once('=') {
                Some((key, value)) => (key, value),
                None => return line.to_string(),
            };
            if should_redact_key_value(key, value) {
                format!("{}\t{}\t{}=[REDACTED]", first, second, key)
            } else {
                line.to_string()
            }
        }
        // 2-field format: origin \t key=value
        // (from `git config --list --show-origin` without --show-scope)
        None => {
            let (key, value) = match second.split_once('=') {
                Some((key, value)) => (key, value),
                None => return line.to_string(),
            };
            if should_redact_key_value(key, value) {
                format!("{}\t{}=[REDACTED]", first, key)
            } else {
                line.to_string()
            }
        }
    }
}

fn should_redact_key_value(key: &str, value: &str) -> bool {
    let key_lower = key.to_lowercase();
    let value_lower = value.to_lowercase();

    let sensitive_key_markers = [
        "password",
        "passwd",
        "token",
        "secret",
        "oauth",
        "authorization",
        "apikey",
        "api_key",
        "extraheader",
    ];

    if sensitive_key_markers
        .iter()
        .any(|marker| key_lower.contains(marker))
    {
        return true;
    }

    if key_lower.starts_with("url.") {
        return true;
    }

    sensitive_key_markers
        .iter()
        .any(|marker| value_lower.contains(marker))
}

fn collect_autter_config_dump() -> Result<String, String> {
    let runtime = config::Config::get();
    let mut out = String::new();
    let config_path = config::config_file_path_public()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unavailable>".to_string());
    let autter_dir = config::autter_dir_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unavailable>".to_string());

    let _ = writeln!(out, "config_file_path: {}", config_path);
    let _ = writeln!(out, "autter_dir: {}", autter_dir);
    let _ = writeln!(out, "runtime_config:");
    let serialized = runtime.to_printable_json_pretty()?;
    append_indented_block(&mut out, &serialized);
    Ok(out)
}

fn collect_git_environment() -> Vec<String> {
    collect_git_environment_entries(env::vars())
}

fn collect_git_environment_entries<I>(entries: I) -> Vec<String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut entries: Vec<(String, String)> = entries
        .into_iter()
        .filter(|(key, _)| is_debug_git_env_key(key))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    entries
        .into_iter()
        .map(|(key, value)| format!("{}={}", key, redact_env_value(&key, &value)))
        .collect()
}

fn is_debug_git_env_key(key: &str) -> bool {
    key.starts_with("AUTTER_") || key.starts_with("AUTTER_") || key.starts_with("GIT_")
}

fn redact_env_value(key: &str, value: &str) -> String {
    let key_lower = key.to_lowercase();
    let sensitive_markers = ["token", "secret", "password", "key"];
    if sensitive_markers
        .iter()
        .any(|marker| key_lower.contains(marker))
    {
        return "[REDACTED]".to_string();
    }

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }

    if trimmed.len() > 200 {
        let truncated: String = trimmed.chars().take(200).collect();
        return format!("{}...[truncated]", truncated);
    }

    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    fn stdout_stderr_sleep_command() -> (&'static str, Vec<&'static str>) {
        (
            "sh",
            vec!["-c", "printf out; printf err >&2; exec sleep 60"],
        )
    }

    #[cfg(windows)]
    fn stdout_stderr_sleep_command() -> (&'static str, Vec<&'static str>) {
        (
            "powershell.exe",
            vec![
                "-NoProfile",
                "-Command",
                "[Console]::Out.Write('out'); [Console]::Error.Write('err'); Start-Sleep -Seconds 60",
            ],
        )
    }

    #[test]
    fn test_redact_git_config_line_redacts_sensitive_key() {
        let line =
            "global\tfile:/Users/me/.gitconfig\thttp.https://example.com/.extraheader=AUTH token";
        let redacted = redact_git_config_line(line);
        assert_eq!(
            redacted,
            "global\tfile:/Users/me/.gitconfig\thttp.https://example.com/.extraheader=[REDACTED]"
        );
    }

    #[test]
    fn test_redact_git_config_line_keeps_non_sensitive_key() {
        let line = "global\tfile:/Users/me/.gitconfig\tcore.editor=vim";
        let redacted = redact_git_config_line(line);
        assert_eq!(redacted, line);
    }

    #[test]
    fn test_redact_git_config_line_two_field_format_redacts_sensitive() {
        // `git config --list --show-origin` (without --show-scope) produces 2-tab fields
        let line =
            "file:/Users/me/.gitconfig\thttp.https://example.com/.extraheader=BEARER secret123";
        let redacted = redact_git_config_line(line);
        assert_eq!(
            redacted,
            "file:/Users/me/.gitconfig\thttp.https://example.com/.extraheader=[REDACTED]"
        );
    }

    #[test]
    fn test_redact_git_config_line_two_field_format_keeps_non_sensitive() {
        let line = "file:/Users/me/.gitconfig\tcore.editor=vim";
        let redacted = redact_git_config_line(line);
        assert_eq!(redacted, line);
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(1024), "1.00 KB (1024 bytes)");
    }

    #[test]
    fn test_is_debug_git_env_key_matches_git_prefixes() {
        assert!(is_debug_git_env_key("AUTTER_DEBUG"));
        assert!(is_debug_git_env_key("AUTTER_TEST_DB_PATH"));
        assert!(is_debug_git_env_key("GIT_DIR"));
        assert!(is_debug_git_env_key("GIT_TRACE2_EVENT"));
        assert!(!is_debug_git_env_key("GITHUB_TOKEN"));
        assert!(!is_debug_git_env_key("PATH"));
    }

    #[test]
    fn test_collect_git_environment_entries_sorts_and_redacts() {
        let entries = collect_git_environment_entries(vec![
            ("OTHER".to_string(), "ignored".to_string()),
            ("GIT_DIR".to_string(), ".git".to_string()),
            ("AUTTER_TEST_DB_PATH".to_string(), "/tmp/db".to_string()),
            ("AUTTER_API_KEY".to_string(), "secret".to_string()),
        ]);

        assert_eq!(
            entries,
            vec![
                "AUTTER_TEST_DB_PATH=/tmp/db",
                "AUTTER_API_KEY=[REDACTED]",
                "GIT_DIR=.git",
            ]
        );
    }

    #[test]
    fn test_parse_git_version_handles_platform_suffixes() {
        assert_eq!(
            parse_git_version("git version 2.54.0.windows.1"),
            Some(GitVersion {
                major: 2,
                minor: 54,
                patch: 0
            })
        );
        assert_eq!(
            parse_git_version("git version 2.39.5 (Apple Git-154)"),
            Some(GitVersion {
                major: 2,
                minor: 39,
                patch: 5
            })
        );
    }

    #[test]
    fn test_parse_git_version_accepts_minimum_version() {
        assert!(parse_git_version("git version 2.22.0").unwrap() >= MIN_GIT_VERSION);
        assert!(parse_git_version("git version 2.21.9").unwrap() < MIN_GIT_VERSION);
    }

    #[test]
    fn test_select_lookup_path_prefers_existing_path() {
        let exe = env::current_exe().unwrap();
        let output = format!("/definitely/not/git\n{}\n", exe.display());

        assert_eq!(
            select_lookup_path(&output).unwrap(),
            exe.display().to_string()
        );
    }

    #[test]
    fn test_select_lookup_path_falls_back_to_first_non_empty_line() {
        assert_eq!(
            select_lookup_path("\n git: aliased to hub \n").unwrap(),
            "git: aliased to hub"
        );
    }

    #[test]
    fn test_realpath_for_display_canonicalizes_existing_path() {
        let exe = env::current_exe().unwrap();
        let expected = fs::canonicalize(&exe).unwrap();

        assert_eq!(
            realpath_for_display(&exe.display().to_string()),
            expected.display().to_string()
        );
    }

    #[test]
    fn test_run_command_capture_with_timeout_reports_partial_output() {
        let (program, args) = stdout_stderr_sleep_command();
        let err = run_command_capture_with_timeout(program, &args, Duration::from_millis(300))
            .unwrap_err();

        assert!(err.contains("timed out after"), "{err}");
        assert!(
            err.contains("sent kill to child process")
                || err.contains("failed to kill child process"),
            "{err}"
        );
        assert!(err.contains("stdout before timeout: out"), "{err}");
        assert!(err.contains("stderr before timeout: err"), "{err}");
    }

    #[test]
    fn test_parse_debug_options_accepts_skip_trace2_checks() {
        let options = parse_debug_options(&[SKIP_TRACE2_CHECKS_FLAG.to_string()]).unwrap();
        assert!(options.skip_trace2_checks);
    }

    #[test]
    fn test_parse_debug_options_rejects_unknown_arg() {
        let err = parse_debug_options(&["--wat".to_string()]).unwrap_err();
        assert!(err.contains("unknown debug argument: --wat"), "{err}");
    }

    #[test]
    fn test_append_diagnostic_check_renders_fix_line_only_when_failed() {
        let failed = DiagnosticCheckResult::failed(
            "trace2 global config is not configured",
            vec!["ERROR: trace2 is not configured".to_string()],
            Vec::new(),
        )
        .with_remediation("run `autter install` to write the required trace2 settings");

        let mut out = String::new();
        append_diagnostic_check(&mut out, "Trace2 config check", &failed, false);
        assert!(
            out.contains("    fix: run `autter install` to write the required trace2 settings"),
            "{out}"
        );

        let skipped = DiagnosticCheckResult::skipped("trace2 check skipped", Vec::new());
        let mut out = String::new();
        append_diagnostic_check(&mut out, "Trace2 config check", &skipped, false);
        assert!(!out.contains("fix:"), "{out}");
    }

    #[test]
    fn test_append_git_committer_identity_includes_effective_author() {
        let identity = GitCommitterIdentityInfo {
            global_config: Ok(GitConfigIdentityResolution {
                raw_name: Some("Git User".to_string()),
                raw_email: Some("git@example.com".to_string()),
                identity: GitAuthorIdentity {
                    name: Some("Git User".to_string()),
                    email: Some("git@example.com".to_string()),
                },
            }),
            repository: RepositoryCommitterIdentity::InRepository(GitIdentityResolution {
                raw_git_var: Some("Git User <git@example.com> 1234567890 +0000".to_string()),
                identity: GitAuthorIdentity {
                    name: Some("Git User".to_string()),
                    email: Some("git@example.com".to_string()),
                },
            }),
            author_config: config::AuthorConfig {
                name: Some("Config User".to_string()),
                email: None,
            },
        };

        let mut out = String::new();
        append_git_committer_identity(&mut out, &identity);

        assert!(out.contains("Autter author config override:"), "{out}");
        assert!(out.contains("  author.name: Config User"), "{out}");
        assert!(out.contains("  author.email: <unset>"), "{out}");
        assert!(out.contains("Autter effective author identity:"), "{out}");
        assert!(
            out.contains("  Formatted: Config User <git@example.com>"),
            "{out}"
        );
    }

    #[test]
    fn test_command_output_to_result_formats_diagnostics_without_stderr() {
        let err = command_output_to_result(TimedCommandOutput {
            status: Some(1),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            diagnostics: vec!["output collection did not finish".to_string()],
            wait_error: None,
        })
        .unwrap_err();

        assert_eq!(err, "exit code 1: output collection did not finish");
    }
}
