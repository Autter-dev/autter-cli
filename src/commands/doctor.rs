//! `autter doctor` -- one-command setup validation.
//!
//! Runs the trust-critical checks (git, config, background service, trace2
//! capture, an end-to-end checkpoint round-trip, AI agent hooks, and
//! login/connectivity) and prints one pass/warn/fail line per check with a
//! concrete fix for every failure. Unlike `autter debug` (a full support
//! dump that always exits 0), `doctor` is focused and scriptable: it exits
//! non-zero when any check fails, and `--json` emits a machine-readable
//! report on a single line.

use crate::api::{ApiClient, ApiContext};
use crate::auth::{AuthState, collect_auth_status, format_unix_timestamp};
use crate::commands::arg_parser::{self, ScanMode};
use crate::config::{self, Config};
use crate::diagnostics::{DiagnosticCheckResult, DiagnosticStatus, GitDiagnosticTarget};
use crate::process_timeout::run_command_with_timeout;
use serde::Serialize;
use std::time::Duration;

const SKIP_TRACE2_CHECKS_FLAG: &str = "--skip-trace2-checks";
const DOCTOR_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const DOCTOR_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CONNECTIVITY_TIMEOUT_SECS: u64 = 5;
/// Cheap unauthenticated endpoint used only as a reachability probe. Keep in
/// sync with `releases_endpoint()` in `upgrade.rs`.
const CONNECTIVITY_PROBE_ENDPOINT: &str = "/worker/releases";

/// Minimum git version required for autter to function correctly. Keep in
/// sync with `MIN_GIT_VERSION` in `install_hooks.rs` and `debug.rs`.
const MIN_GIT_VERSION: (u32, u32, u32) = (2, 22, 0);
const MIN_GIT_VERSION_DISPLAY: &str = "2.22.0";

const SECTION_SETUP: &str = "Setup";
const SECTION_SERVICE: &str = "Background service";
const SECTION_TRACE2: &str = "Git capture (trace2)";
const SECTION_E2E: &str = "End-to-end checkpoint";
const SECTION_AGENTS: &str = "AI agent hooks";
const SECTION_ACCOUNT: &str = "Account & sync";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum DoctorStatus {
    Passed,
    Warning,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheck {
    section: &'static str,
    name: String,
    status: DoctorStatus,
    summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    details: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remediation: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct DoctorSummary {
    passed: usize,
    warnings: usize,
    failed: usize,
    skipped: usize,
}

#[derive(Debug, Serialize)]
struct DoctorOutput {
    autter_version: String,
    checks: Vec<DoctorCheck>,
    summary: DoctorSummary,
    ok: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct DoctorOptions {
    skip_trace2_checks: bool,
}

pub fn handle_doctor(args: &[String]) {
    let pp = match arg_parser::pre_parse(args, ScanMode::Full, false) {
        Ok(pp) => pp,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    };
    if pp.flags.help {
        arg_parser::print_command_help("doctor");
        return;
    }
    arg_parser::merge_global_flags(&pp.flags);

    let mut options = DoctorOptions::default();
    for arg in &pp.rest {
        match arg.as_str() {
            SKIP_TRACE2_CHECKS_FLAG => options.skip_trace2_checks = true,
            "help" => {
                arg_parser::print_command_help("doctor");
                return;
            }
            unknown => {
                eprintln!("error: unknown doctor argument: {}", unknown);
                arg_parser::print_command_help("doctor");
                std::process::exit(crate::commands::EXIT_USAGE_ERROR);
            }
        }
    }

    let output = run_doctor(&options);
    if output.summary.failed > 0 {
        std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
    }
}

fn run_doctor(options: &DoctorOptions) -> DoctorOutput {
    let json = arg_parser::json();
    let mut reporter = Reporter::new(json);

    if !json {
        println!("autter doctor (autter {})", version_string());
        println!("Validating your autter setup -- the end-to-end checks take a few seconds.");
    }

    let git_cmd = Config::get().git_cmd().to_string();

    // Setup: git binary/version, config file parse, repository eligibility.
    let (git_check, terminal_git_ok) = check_git_versions(&git_cmd);
    reporter.add(git_check);
    reporter.add(check_config_file());
    reporter.add(check_repository());

    // Background service: readiness plus a real trace2 ingestion probe. This
    // starts (or restarts) the daemon when needed, so it must run before the
    // end-to-end checkpoint check.
    let mut daemon_check = check_from_diagnostic(
        SECTION_SERVICE,
        "background service",
        crate::diagnostics::prepare_daemon_for_debug_self_checks(&git_cmd),
    );
    if daemon_check.status == DoctorStatus::Passed {
        daemon_check.summary =
            "background service is running and ingesting git trace2 events".to_string();
    }
    let daemon_ok = daemon_check.status != DoctorStatus::Failed;
    reporter.add(daemon_check);

    // Git capture: the trace2 global config every git invocation depends on,
    // checked for both the git autter runs and the git on the user's PATH.
    let configured_target = GitDiagnosticTarget::new("configured git", &git_cmd);
    let terminal_target = GitDiagnosticTarget::new("terminal git", "git");
    reporter.add(check_from_diagnostic(
        SECTION_TRACE2,
        "trace2 config (configured git)",
        crate::diagnostics::check_trace2_global_config(&configured_target),
    ));
    reporter.add(check_from_diagnostic(
        SECTION_TRACE2,
        "trace2 config (terminal git)",
        crate::diagnostics::check_trace2_global_config(&terminal_target),
    ));
    if options.skip_trace2_checks {
        reporter.add(skipped_check(
            SECTION_TRACE2,
            "trace2 event capture",
            format!("skipped ({})", SKIP_TRACE2_CHECKS_FLAG),
        ));
    } else {
        reporter.add(check_from_diagnostic(
            SECTION_TRACE2,
            "trace2 event capture",
            crate::diagnostics::run_trace2_file_self_check(&configured_target),
        ));
    }

    // End-to-end: a real checkpoint event through the full pipeline --
    // `autter checkpoint` -> background service -> working log -> commit ->
    // attribution. Runs against the terminal git when available since that is
    // what the user's own commits go through.
    if daemon_ok {
        let e2e_target = if terminal_git_ok {
            &terminal_target
        } else {
            &configured_target
        };
        let mut e2e_check = check_from_diagnostic(
            SECTION_E2E,
            "checkpoint round-trip",
            crate::diagnostics::run_attribution_self_check(e2e_target),
        );
        if e2e_check.status == DoctorStatus::Passed {
            e2e_check.summary = format!(
                "checkpoint -> service -> commit -> attribution round-trip succeeded ({})",
                e2e_target.label
            );
        }
        reporter.add(e2e_check);
    } else {
        reporter.add(skipped_check(
            SECTION_E2E,
            "checkpoint round-trip",
            "skipped -- fix the background service check first",
        ));
    }

    // AI agent hooks: would an edit made right now actually checkpoint?
    for check in agent_hook_checks() {
        reporter.add(check);
    }
    if let Some(check) = vscode_native_hooks_check() {
        reporter.add(check);
    }

    // Account & sync: auth state and API reachability. Uploads silently
    // no-op when unauthenticated, so doctor is where a broken login surfaces.
    let (auth_check, authenticated) = check_auth();
    reporter.add(auth_check);
    reporter.add(check_connectivity(authenticated));
    reporter.add(check_org_data_plane());
    reporter.add(check_sync_queue());

    reporter.finish()
}

// ===========================================================================
// Individual checks
// ===========================================================================

/// Check the configured git binary (and the terminal `git` on PATH) run and
/// meet the minimum supported version. Returns the check plus whether the
/// terminal git is usable, which decides the end-to-end check's git target.
fn check_git_versions(git_cmd: &str) -> (DoctorCheck, bool) {
    let mut details = Vec::new();
    let mut status = DoctorStatus::Passed;
    let mut summary;
    let mut remediation = None;

    match git_version_of(git_cmd) {
        Ok((raw, parsed)) => {
            details.push(format!("configured git: {} ({})", git_cmd, raw));
            match parsed {
                Some(version) if version >= MIN_GIT_VERSION => {
                    summary = format!(
                        "git {}.{}.{} meets the {} minimum",
                        version.0, version.1, version.2, MIN_GIT_VERSION_DISPLAY
                    );
                }
                Some(version) => {
                    status = DoctorStatus::Failed;
                    summary = format!(
                        "git {}.{}.{} is below the {} minimum -- attribution will not work",
                        version.0, version.1, version.2, MIN_GIT_VERSION_DISPLAY
                    );
                    remediation = Some(format!(
                        "upgrade git to {} or newer, then re-run `autter doctor`",
                        MIN_GIT_VERSION_DISPLAY
                    ));
                }
                None => {
                    status = DoctorStatus::Warning;
                    summary = format!(
                        "could not parse the git version from '{}' (minimum is {})",
                        raw, MIN_GIT_VERSION_DISPLAY
                    );
                }
            }
        }
        Err(err) => {
            status = DoctorStatus::Failed;
            summary = "the configured git binary could not be run".to_string();
            details.push(format!("configured git: {} ({})", git_cmd, err));
            remediation = Some(
                "point autter at a working git binary (`autter config set git_path <path>`), then re-run `autter doctor`"
                    .to_string(),
            );
        }
    }

    let terminal_git_ok = match git_version_of("git") {
        Ok((raw, parsed)) => {
            details.push(format!("terminal git: {}", raw));
            if let Some(version) = parsed
                && version < MIN_GIT_VERSION
                && status != DoctorStatus::Failed
            {
                status = DoctorStatus::Failed;
                summary = format!(
                    "the git on your PATH ({}.{}.{}) is below the {} minimum -- your own git commands will not be captured",
                    version.0, version.1, version.2, MIN_GIT_VERSION_DISPLAY
                );
                remediation = Some(format!(
                    "upgrade git to {} or newer, then re-run `autter doctor`",
                    MIN_GIT_VERSION_DISPLAY
                ));
            }
            true
        }
        Err(err) => {
            details.push(format!("terminal git: not runnable ({})", err));
            if status == DoctorStatus::Passed {
                status = DoctorStatus::Warning;
                summary.push_str(" -- but no runnable `git` was found on PATH");
            }
            false
        }
    };

    (
        DoctorCheck {
            section: SECTION_SETUP,
            name: "git version".to_string(),
            status,
            summary,
            details,
            remediation,
        },
        terminal_git_ok,
    )
}

/// Raw `--version` output plus the parsed (major, minor, patch), when the
/// output was parseable.
type GitVersionProbe = (String, Option<(u32, u32, u32)>);

fn git_version_of(program: &str) -> Result<GitVersionProbe, String> {
    let output = run_command_with_timeout(
        program,
        &["--version"],
        None,
        DOCTOR_COMMAND_TIMEOUT,
        DOCTOR_COMMAND_POLL_INTERVAL,
        &[],
    )?;
    if output.timed_out {
        return Err("timed out".to_string());
    }
    if output.status != Some(0) {
        return Err(format!(
            "exit status {}: {}",
            output
                .status
                .map(|code| code.to_string())
                .unwrap_or_else(|| "<unavailable>".to_string()),
            output.stderr
        ));
    }
    let raw = output.stdout.trim().to_string();
    let parsed = crate::mdm::utils::parse_version_triple(&raw);
    Ok((raw, parsed))
}

/// Surface a malformed `~/.autter/config.json`. The normal runtime load path
/// silently falls back to defaults when the file cannot be parsed, so a typo
/// there means every setting in the file is ignored without any signal --
/// this check is where that failure mode becomes visible.
fn check_config_file() -> DoctorCheck {
    let name = "configuration file".to_string();
    let Some(path) = config::config_file_path_public() else {
        return DoctorCheck {
            section: SECTION_SETUP,
            name,
            status: DoctorStatus::Failed,
            summary: "could not determine the config file path".to_string(),
            details: Vec::new(),
            remediation: Some(
                "check that your home directory is set (HOME on macOS/Linux, USERPROFILE on Windows), then re-run `autter doctor`"
                    .to_string(),
            ),
        };
    };

    if !path.exists() {
        return DoctorCheck {
            section: SECTION_SETUP,
            name,
            status: DoctorStatus::Passed,
            summary: "no config file present (defaults in use)".to_string(),
            details: vec![format!("path: {}", path.display())],
            remediation: None,
        };
    }

    match config::load_file_config_public() {
        Ok(_) => DoctorCheck {
            section: SECTION_SETUP,
            name,
            status: DoctorStatus::Passed,
            summary: "config file is valid".to_string(),
            details: vec![format!("path: {}", path.display())],
            remediation: None,
        },
        Err(err) => DoctorCheck {
            section: SECTION_SETUP,
            name,
            status: DoctorStatus::Failed,
            summary: "config file could not be parsed -- autter is silently running with defaults"
                .to_string(),
            details: vec![format!("path: {}", path.display()), err],
            remediation: Some(format!(
                "fix or remove {} (autter ignores an unparseable config without warning), then re-run `autter doctor`",
                path.display()
            )),
        },
    }
}

/// When run inside a repository, verify the allow/exclude repository filters
/// do not block it -- a filtered-out repo records no attribution at all.
fn check_repository() -> DoctorCheck {
    let name = "repository".to_string();
    let Ok(repo) = crate::git::find_repository_in_path(".") else {
        return skipped_check(
            SECTION_SETUP,
            "repository",
            "not inside a git repository (repository checks skipped)",
        );
    };

    let workdir = repo
        .workdir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let config = Config::get();

    if !config.has_repository_filters() {
        return DoctorCheck {
            section: SECTION_SETUP,
            name,
            status: DoctorStatus::Passed,
            summary: "repository is eligible for attribution capture".to_string(),
            details: vec![format!("workdir: {}", workdir)],
            remediation: None,
        };
    }

    if config.is_allowed_repository(&Some(repo)) {
        DoctorCheck {
            section: SECTION_SETUP,
            name,
            status: DoctorStatus::Passed,
            summary: "repository matches your allow/exclude repository filters".to_string(),
            details: vec![format!("workdir: {}", workdir)],
            remediation: None,
        }
    } else {
        DoctorCheck {
            section: SECTION_SETUP,
            name,
            status: DoctorStatus::Failed,
            summary:
                "this repository is blocked by allow_repositories/exclude_repositories -- autter will not capture attribution here"
                    .to_string(),
            details: vec![format!("workdir: {}", workdir)],
            remediation: Some(
                "update allow_repositories / exclude_repositories via `autter config` if this repository should be tracked"
                    .to_string(),
            ),
        }
    }
}

/// Per-agent hook status from the same installers `autter install-hooks`
/// manages. Agents that are not installed on this machine are collapsed into
/// a single skipped line so detected problems stay prominent.
fn agent_hook_checks() -> Vec<DoctorCheck> {
    use crate::mdm::agents::get_all_installers;
    use crate::mdm::hook_installer::HookInstallerParams;

    let binary_path =
        std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("autter"));
    let params = HookInstallerParams { binary_path };

    let mut checks = Vec::new();
    let mut not_detected = Vec::new();

    for installer in get_all_installers() {
        let name = installer.name().to_string();
        match installer.check_hooks(&params) {
            Ok(result) if !result.tool_installed => not_detected.push(name),
            Ok(result) if installer.id() == "vscode" => {
                // VS Code has no config-file hooks; hooks_installed reflects
                // the autter extension, which carries manual-edit capture and
                // (on older VS Code) Copilot AI-edit capture.
                if result.hooks_installed {
                    checks.push(DoctorCheck {
                        section: SECTION_AGENTS,
                        name,
                        status: DoctorStatus::Passed,
                        summary: "autter extension installed".to_string(),
                        details: Vec::new(),
                        remediation: None,
                    });
                } else {
                    checks.push(DoctorCheck {
                        section: SECTION_AGENTS,
                        name,
                        status: DoctorStatus::Failed,
                        summary:
                            "autter extension NOT installed -- manual edits (and Copilot edits on older VS Code) are not captured"
                                .to_string(),
                        details: Vec::new(),
                        remediation: Some(
                            "run `autter install-hooks`, then restart VS Code".to_string(),
                        ),
                    });
                }
            }
            Ok(result) if !result.hooks_installed => checks.push(DoctorCheck {
                section: SECTION_AGENTS,
                name: name.clone(),
                status: DoctorStatus::Failed,
                summary:
                    "detected, but autter hooks are NOT installed -- its edits are not captured"
                        .to_string(),
                details: Vec::new(),
                remediation: Some(format!("run `autter install-hooks`, then restart {}", name)),
            }),
            Ok(result) if result.hooks_up_to_date => checks.push(DoctorCheck {
                section: SECTION_AGENTS,
                name,
                status: DoctorStatus::Passed,
                summary: "hooks installed (up to date)".to_string(),
                details: Vec::new(),
                remediation: None,
            }),
            Ok(_) => checks.push(DoctorCheck {
                section: SECTION_AGENTS,
                name: name.clone(),
                status: DoctorStatus::Warning,
                summary: "hooks installed but out of date".to_string(),
                details: Vec::new(),
                remediation: Some(format!(
                    "run `autter install-hooks` to refresh them, then restart {}",
                    name
                )),
            }),
            Err(err) => checks.push(DoctorCheck {
                section: SECTION_AGENTS,
                name,
                status: DoctorStatus::Failed,
                summary: format!("hook status check failed: {}", err),
                details: Vec::new(),
                remediation: Some(
                    "resolve the issue above, then run `autter install-hooks`".to_string(),
                ),
            }),
        }
    }

    if checks.is_empty() && !not_detected.is_empty() {
        checks.push(DoctorCheck {
            section: SECTION_AGENTS,
            name: "AI agents".to_string(),
            status: DoctorStatus::Warning,
            summary: "no supported AI coding agents detected on this machine".to_string(),
            details: vec![format!("looked for: {}", not_detected.join(", "))],
            remediation: Some(
                "for remote dev (SSH/WSL/devcontainers), install autter and run `autter install-hooks` on the machine where your agents run"
                    .to_string(),
            ),
        });
    } else if !not_detected.is_empty() {
        checks.push(skipped_check(
            SECTION_AGENTS,
            "not detected",
            not_detected.join(", "),
        ));
    }

    checks
}

/// The VS Code >= 1.109.3 native-hooks chain: on those builds the autter
/// extension defers AI-edit capture to VS Code itself, so capture silently
/// stops unless the hook file and chat settings are all in place.
fn vscode_native_hooks_check() -> Option<DoctorCheck> {
    use crate::commands::debug::{VsCodeChainOutcome, inspect_vscode_native_hooks_chain};

    let chain = inspect_vscode_native_hooks_chain();
    let name = "VS Code Copilot native hooks".to_string();
    match chain.outcome {
        VsCodeChainOutcome::NotDetected => None,
        VsCodeChainOutcome::VersionUnknown => Some(DoctorCheck {
            section: SECTION_AGENTS,
            name,
            status: DoctorStatus::Warning,
            summary: "could not determine the VS Code version".to_string(),
            details: chain.lines,
            remediation: None,
        }),
        VsCodeChainOutcome::LegacyExtensionMode => Some(DoctorCheck {
            section: SECTION_AGENTS,
            name,
            status: DoctorStatus::Passed,
            summary: "VS Code predates native agent hooks -- the autter extension handles capture"
                .to_string(),
            details: chain.lines,
            remediation: None,
        }),
        VsCodeChainOutcome::Complete => Some(DoctorCheck {
            section: SECTION_AGENTS,
            name,
            status: DoctorStatus::Passed,
            summary:
                "Copilot agent-mode capture chain is complete (restart VS Code if hooks were just installed)"
                    .to_string(),
            details: chain.lines,
            remediation: None,
        }),
        VsCodeChainOutcome::Incomplete => Some(DoctorCheck {
            section: SECTION_AGENTS,
            name,
            status: DoctorStatus::Failed,
            summary: "Copilot agent-mode edits are likely NOT being captured".to_string(),
            details: chain.lines,
            remediation: Some("run `autter install-hooks` and restart VS Code".to_string()),
        }),
    }
}

/// Auth state check. Returns the check plus whether the user is authenticated
/// for sync (logged in or API key), which gates the connectivity probe.
fn check_auth() -> (DoctorCheck, bool) {
    let name = "authentication".to_string();
    let status = collect_auth_status();
    let has_api_key = Config::get().api_key().is_some();
    let mut details = vec![format!("credential backend: {}", status.backend)];
    let live_context = ApiContext::new(None);
    let live_token = live_context.auth_token.is_some();
    if let Some(issue) = &live_context.auth_issue {
        details.push(format!("credential check: {issue}"));
    }

    match status.state {
        AuthState::LoggedIn if live_token => {
            let who = status
                .email
                .or(status.name)
                .or(status.user_id)
                .unwrap_or_else(|| "<unknown user>".to_string());
            if let Some(expiry) = status.refresh_token_expires_at {
                details.push(format!("session valid until: {}", format_unix_timestamp(expiry)));
            }
            if has_api_key {
                details.push("API key also configured".to_string());
            }
            (
                DoctorCheck {
                    section: SECTION_ACCOUNT,
                    name,
                    status: DoctorStatus::Passed,
                    summary: format!("logged in as {}", who),
                    details,
                    remediation: None,
                },
                true,
            )
        }
        AuthState::LoggedIn => (
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: DoctorStatus::Failed,
                summary: "stored login could not produce a usable access token".to_string(),
                details,
                remediation: Some("run `autter login`, then `autter bg restart`".to_string()),
            },
            has_api_key,
        ),
        AuthState::SyncBlocked if live_token => (
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: DoctorStatus::Warning,
                summary: "a previous sync was authentication-blocked; credentials now load"
                    .to_string(),
                details,
                remediation: Some(
                    "let the data-plane check finish; restart with `autter bg restart` if the queue remains blocked"
                        .to_string(),
                ),
            },
            true,
        ),
        AuthState::SyncBlocked => (
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: DoctorStatus::Failed,
                summary: "cloud sync is blocked because stored credentials cannot authenticate"
                    .to_string(),
                details,
                remediation: Some("run `autter login`, then `autter bg restart`".to_string()),
            },
            has_api_key,
        ),
        AuthState::RefreshExpired => (
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: DoctorStatus::Failed,
                summary:
                    "login session expired -- cloud sync is silently disabled until you log in again"
                        .to_string(),
                details,
                remediation: Some("run `autter login` to re-authenticate".to_string()),
            },
            has_api_key,
        ),
        AuthState::Error(err) => (
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: DoctorStatus::Failed,
                summary: format!("could not read stored credentials: {}", err),
                details,
                remediation: Some("run `autter login` to re-authenticate".to_string()),
            },
            has_api_key,
        ),
        AuthState::LoggedOut if has_api_key => (
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: DoctorStatus::Passed,
                summary: "authenticated via configured API key".to_string(),
                details,
                remediation: None,
            },
            true,
        ),
        AuthState::LoggedOut => (
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: DoctorStatus::Warning,
                summary: "not logged in -- attribution is captured locally but never syncs"
                    .to_string(),
                details,
                remediation: Some(
                    "run `autter login` to enable cloud sync (ignore this if you use autter locally only)"
                        .to_string(),
                ),
            },
            false,
        ),
    }
}

/// Probe the API base URL. Any HTTP response (including 4xx/5xx) proves the
/// service is reachable; only transport failures (DNS, TLS, proxy, firewall)
/// fail the check.
fn check_connectivity(authenticated: bool) -> DoctorCheck {
    let name = "cloud connectivity".to_string();
    let base_url = Config::get()
        .api_base_url()
        .trim_end_matches('/')
        .to_string();

    if !authenticated {
        return skipped_check(
            SECTION_ACCOUNT,
            "cloud connectivity",
            "not authenticated -- skipping the API reachability probe",
        );
    }

    let url = format!("{}{}", base_url, CONNECTIVITY_PROBE_ENDPOINT);
    let agent = crate::http::build_agent(Some(CONNECTIVITY_TIMEOUT_SECS));
    match crate::http::send(agent.get(&url)) {
        Ok(response) => DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Passed,
            summary: format!("API reachable (HTTP {})", response.status_code),
            details: vec![format!("url: {}", url)],
            remediation: None,
        },
        Err(err) => DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Failed,
            summary: "could not reach the autter API -- sync and uploads will fail".to_string(),
            details: vec![format!("url: {}", url), format!("error: {}", err)],
            remediation: Some(format!(
                "check network/proxy/firewall access to {}, then re-run `autter doctor`",
                base_url
            )),
        },
    }
}

/// Probe the actual per-organization PostgreSQL data plane. API reachability is
/// insufficient: login can work while the database route, TLS connection, or
/// cached client used by uploads is broken.
fn check_org_data_plane() -> DoctorCheck {
    let name = "organization data plane".to_string();
    let client = ApiClient::new(ApiContext::new(None));
    if !client.is_logged_in() {
        let reason = client
            .auth_issue()
            .map(ToString::to_string)
            .unwrap_or_else(|| "no access token".to_string());
        return DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Warning,
            summary: "data-plane probe skipped because no usable login is available".to_string(),
            details: vec![format!("reason: {reason}")],
            remediation: Some("run `autter login`, then re-run `autter doctor`".to_string()),
        };
    }

    match client.check_org_data_plane() {
        Ok(()) => {
            crate::auth::notice::clear_sync_auth_blocked();
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: DoctorStatus::Passed,
                summary: "organization database is reachable by the upload path".to_string(),
                details: Vec::new(),
                remediation: None,
            }
        }
        Err(error) => DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Failed,
            summary: "could not reach the organization database used for uploads".to_string(),
            details: vec![format!("error: {error}")],
            remediation: Some(
                "check network access to the organization database, then run `autter bg restart` and `autter doctor`"
                    .to_string(),
            ),
        },
    }
}

fn check_sync_queue() -> DoctorCheck {
    let name = "durable sync queue".to_string();
    let pending = crate::auth::notice::pending_sync_counts();
    let details = vec![pending.summary()];
    if pending.total() == 0 {
        return DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Passed,
            summary: "all local cloud-sync queues are empty".to_string(),
            details,
            remediation: None,
        };
    }

    if crate::auth::notice::sync_auth_blocked_recently() {
        DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Failed,
            summary: "queued data is not draining because cloud sync is authentication-blocked"
                .to_string(),
            details,
            remediation: Some("run `autter login`, then `autter bg restart`".to_string()),
        }
    } else {
        DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Warning,
            summary: "local data is queued for background upload".to_string(),
            details,
            remediation: Some(
                "keep the background service running; re-run `autter doctor` if these counts do not decrease"
                    .to_string(),
            ),
        }
    }
}

// ===========================================================================
// Plumbing
// ===========================================================================

fn check_from_diagnostic(
    section: &'static str,
    name: &str,
    result: DiagnosticCheckResult,
) -> DoctorCheck {
    let status = match result.status {
        DiagnosticStatus::Passed => DoctorStatus::Passed,
        DiagnosticStatus::Failed => DoctorStatus::Failed,
        DiagnosticStatus::Skipped => DoctorStatus::Skipped,
    };
    DoctorCheck {
        section,
        name: name.to_string(),
        status,
        summary: result.summary,
        details: result.details,
        remediation: result.remediation,
    }
}

fn skipped_check(
    section: &'static str,
    name: impl Into<String>,
    summary: impl Into<String>,
) -> DoctorCheck {
    DoctorCheck {
        section,
        name: name.into(),
        status: DoctorStatus::Skipped,
        summary: summary.into(),
        details: Vec::new(),
        remediation: None,
    }
}

fn version_string() -> String {
    if cfg!(debug_assertions) {
        format!("{} (debug)", env!("CARGO_PKG_VERSION"))
    } else {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

fn summarize(checks: &[DoctorCheck]) -> DoctorSummary {
    let mut summary = DoctorSummary {
        passed: 0,
        warnings: 0,
        failed: 0,
        skipped: 0,
    };
    for check in checks {
        match check.status {
            DoctorStatus::Passed => summary.passed += 1,
            DoctorStatus::Warning => summary.warnings += 1,
            DoctorStatus::Failed => summary.failed += 1,
            DoctorStatus::Skipped => summary.skipped += 1,
        }
    }
    summary
}

/// Streams human output check-by-check as results come in (the slow checks
/// poll for a few seconds), or collects everything for a single-line JSON
/// document at the end.
struct Reporter {
    json: bool,
    current_section: Option<&'static str>,
    checks: Vec<DoctorCheck>,
}

impl Reporter {
    fn new(json: bool) -> Self {
        Self {
            json,
            current_section: None,
            checks: Vec::new(),
        }
    }

    fn add(&mut self, check: DoctorCheck) {
        if !self.json {
            if self.current_section != Some(check.section) {
                println!();
                println!("{}", check.section);
                self.current_section = Some(check.section);
            }
            print_human_check(&check);
        }
        self.checks.push(check);
    }

    fn finish(self) -> DoctorOutput {
        let summary = summarize(&self.checks);
        let output = DoctorOutput {
            autter_version: version_string(),
            checks: self.checks,
            summary,
            ok: summary.failed == 0,
        };

        if self.json {
            match serde_json::to_string(&output) {
                Ok(serialized) => println!("{}", serialized),
                Err(err) => {
                    eprintln!("Error: failed to serialize doctor report: {}", err);
                    std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
                }
            }
            return output;
        }

        println!();
        let mut parts = vec![arg_parser::paint(
            "1;32",
            &format!("{} passed", summary.passed),
        )];
        if summary.warnings > 0 {
            let label = if summary.warnings == 1 {
                "warning"
            } else {
                "warnings"
            };
            parts.push(arg_parser::paint(
                "1;33",
                &format!("{} {}", summary.warnings, label),
            ));
        }
        if summary.failed > 0 {
            parts.push(arg_parser::paint(
                "1;31",
                &format!("{} failed", summary.failed),
            ));
        }
        if summary.skipped > 0 {
            parts.push(arg_parser::paint(
                "90",
                &format!("{} skipped", summary.skipped),
            ));
        }
        println!("Summary: {}", parts.join(", "));

        if summary.failed > 0 {
            println!("Fixes are listed next to each failed check above.");
            println!("For a full support report to share, run: autter debug");
        } else if summary.warnings > 0 {
            println!("No failures. Review the warnings above if capture or sync seems off.");
        } else {
            println!("All checks passed -- autter is capturing and attributing correctly.");
        }

        output
    }
}

fn print_human_check(check: &DoctorCheck) {
    let (symbol, color) = match check.status {
        DoctorStatus::Passed => ("✓", "1;32"),
        DoctorStatus::Warning => ("⚠", "1;33"),
        DoctorStatus::Failed => ("✗", "1;31"),
        DoctorStatus::Skipped => ("-", "90"),
    };
    println!(
        "  {} {}: {}",
        arg_parser::paint(color, symbol),
        check.name,
        check.summary
    );
    if check.status != DoctorStatus::Passed
        && let Some(remediation) = &check.remediation
    {
        println!("      fix: {}", remediation);
    }
    if matches!(check.status, DoctorStatus::Failed | DoctorStatus::Warning) {
        for detail in &check.details {
            println!("      {}", detail);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_check(status: DoctorStatus) -> DoctorCheck {
        DoctorCheck {
            section: SECTION_SETUP,
            name: "example".to_string(),
            status,
            summary: "summary".to_string(),
            details: Vec::new(),
            remediation: None,
        }
    }

    #[test]
    fn summarize_counts_each_status() {
        let checks = vec![
            make_check(DoctorStatus::Passed),
            make_check(DoctorStatus::Passed),
            make_check(DoctorStatus::Warning),
            make_check(DoctorStatus::Failed),
            make_check(DoctorStatus::Skipped),
        ];
        let summary = summarize(&checks);
        assert_eq!(summary.passed, 2);
        assert_eq!(summary.warnings, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 1);
    }

    #[test]
    fn diagnostic_statuses_map_to_doctor_statuses() {
        let diagnostic = DiagnosticCheckResult {
            status: DiagnosticStatus::Failed,
            summary: "broken".to_string(),
            details: vec!["detail".to_string()],
            commands: Vec::new(),
            trace2_json: None,
            remediation: Some("fix it".to_string()),
        };
        let check = check_from_diagnostic(SECTION_SERVICE, "background service", diagnostic);
        assert_eq!(check.status, DoctorStatus::Failed);
        assert_eq!(check.summary, "broken");
        assert_eq!(check.remediation.as_deref(), Some("fix it"));

        let skipped = DiagnosticCheckResult {
            status: DiagnosticStatus::Skipped,
            summary: "skipped".to_string(),
            details: Vec::new(),
            commands: Vec::new(),
            trace2_json: None,
            remediation: None,
        };
        assert_eq!(
            check_from_diagnostic(SECTION_TRACE2, "x", skipped).status,
            DoctorStatus::Skipped
        );
    }

    #[test]
    fn json_output_shape_is_stable() {
        let checks = vec![make_check(DoctorStatus::Passed), {
            let mut failed = make_check(DoctorStatus::Failed);
            failed.remediation = Some("do the thing".to_string());
            failed
        }];
        let summary = summarize(&checks);
        let output = DoctorOutput {
            autter_version: "test".to_string(),
            checks,
            summary,
            ok: summary.failed == 0,
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&output).unwrap()).unwrap();
        assert!(!value["ok"].as_bool().unwrap());
        assert_eq!(value["summary"]["passed"], 1);
        assert_eq!(value["summary"]["failed"], 1);
        assert_eq!(value["checks"][0]["status"], "passed");
        assert_eq!(value["checks"][1]["status"], "failed");
        assert_eq!(value["checks"][1]["remediation"], "do the thing");
        // Passed checks with no remediation omit the key entirely.
        assert!(value["checks"][0].get("remediation").is_none());
    }
}
