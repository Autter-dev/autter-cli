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
use std::io::IsTerminal;
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

const REMEDIATION_TRACE2_THEN_ATTRIBUTION: &str = "a trace2 check failed above — attribution depends on it. run `autter install --system`, then `autter doctor`";

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
    /// The one command to run next: the first failed check's remediation, or
    /// the first warning when nothing failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_step: Option<String>,
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

    if reporter.live {
        eprintln!();
        crate::ui::Header::new(
            "Autter doctor",
            format!("v{}", version_string()),
            "Checking your setup \u{00B7} the end-to-end checks take a few seconds",
            crate::ui::header::display_cwd(),
        )
        .print();
    } else if !json {
        println!("autter doctor (autter {})", version_string());
        println!("Validating your autter setup -- the end-to-end checks take a few seconds.");
    }

    let git_cmd = Config::get().git_cmd().to_string();
    // A trace2 config autter wrote earlier that points at an old service
    // location is repaired in place (the user consented when it was written),
    // before any check that sends a git event through it. Missing or foreign
    // config is only reported: that needs `autter install --system`.
    let trace2_repair = crate::diagnostics::repair_stale_trace2_config(&git_cmd);
    // Older installs don't report cloud-agent variables per command; add them so
    // agent attribution never depends on the service's own environment.
    let agent_env_repair = crate::diagnostics::repair_trace2_agent_env_vars(&git_cmd);

    // Setup: git binary/version, config file parse, repository eligibility.
    reporter.begin(SECTION_SETUP, "checking git");
    let (git_check, terminal_git_ok) = check_git_versions(&git_cmd);
    reporter.add(git_check);
    reporter.begin(SECTION_SETUP, "reading the config file");
    reporter.add(check_config_file());
    reporter.begin(SECTION_SETUP, "checking this repository");
    reporter.add(check_repository());

    // Background service health uses the same probe as `autter bg status`, so
    // the two commands always agree. This starts (or restarts) the service
    // when needed, so it must run before the end-to-end checkpoint check.
    reporter.begin(SECTION_SERVICE, "checking the background service");
    // What the service looked like before doctor touched it: a stopped or hung
    // service that doctor restarts must not read as a plain pass, because
    // commits made while it was down weren't attributed.
    let initial_service_state = crate::daemon::DaemonConfig::from_env_or_default_paths()
        .ok()
        .map(|config| crate::commands::daemon::probe_daemon_health(&config));
    let mut daemon_check = check_from_diagnostic(
        SECTION_SERVICE,
        "background service",
        crate::diagnostics::ensure_daemon_ready_for_self_checks(),
    );
    if daemon_check.status == DoctorStatus::Passed
        && let Some(initial) = initial_service_state.as_ref()
        && matches!(
            initial.state,
            crate::commands::daemon::DaemonHealthState::NotRunning
                | crate::commands::daemon::DaemonHealthState::Unresponsive
        )
    {
        daemon_check.status = DoctorStatus::Warning;
        daemon_check.summary = format!(
            "{} -- doctor restarted it; commits made while it was down are checked under \"missed commits\"",
            initial.summary
        );
    }
    // Skipped means the service is intentionally not running (before
    // onboarding), so there is nothing to send a git event to.
    let daemon_check_skipped = daemon_check.status == DoctorStatus::Skipped;
    let daemon_ok = !matches!(
        daemon_check.status,
        DoctorStatus::Failed | DoctorStatus::Skipped
    );
    reporter.add(daemon_check);

    // Separately: do real git trace2 events reach the service? A failure here
    // is usually missing trace2 config, not an unhealthy service.
    if daemon_ok {
        reporter.begin(SECTION_SERVICE, "sending a git event to the service");
        reporter.add(check_from_diagnostic(
            SECTION_SERVICE,
            "trace2 ingestion",
            crate::diagnostics::check_daemon_trace2_ingestion(&git_cmd),
        ));
    } else {
        reporter.add(skipped_check(
            SECTION_SERVICE,
            "trace2 ingestion",
            if daemon_check_skipped {
                "skipped -- the background service starts after `autter onboard`"
            } else {
                "skipped -- fix the background service check first"
            },
        ));
    }

    // Git capture: the trace2 global config every git invocation depends on,
    // checked for both the git autter runs and the git on the user's PATH.
    let configured_target = GitDiagnosticTarget::new("configured git", &git_cmd);
    let terminal_target = GitDiagnosticTarget::new("terminal git", "git");
    let mut trace2_blocking = false;
    for (name, target) in [
        ("trace2 config (configured git)", &configured_target),
        ("trace2 config (terminal git)", &terminal_target),
    ] {
        reporter.begin(SECTION_TRACE2, &format!("checking {name}"));
        let mut check = check_from_diagnostic(
            SECTION_TRACE2,
            name,
            crate::diagnostics::check_trace2_global_config(target),
        );
        for repair in [&agent_env_repair, &trace2_repair].into_iter().flatten() {
            check.details.insert(0, repair.clone());
        }
        if check.status == DoctorStatus::Failed {
            trace2_blocking = true;
        }
        reporter.add(check);
    }
    if options.skip_trace2_checks {
        reporter.add(skipped_check(
            SECTION_TRACE2,
            "trace2 event capture",
            format!("skipped ({})", SKIP_TRACE2_CHECKS_FLAG),
        ));
    } else {
        reporter.begin(SECTION_TRACE2, "capturing a real git event");
        let capture = check_from_diagnostic(
            SECTION_TRACE2,
            "trace2 event capture",
            crate::diagnostics::run_trace2_file_self_check(&configured_target),
        );
        if capture.status == DoctorStatus::Failed {
            trace2_blocking = true;
        }
        reporter.add(capture);
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
        reporter.begin(SECTION_E2E, "running a checkpoint through a test commit");
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
        } else if e2e_check.status == DoctorStatus::Failed && trace2_blocking {
            // Attribution cannot work without trace2; steer the user to the
            // failed checks above before the stage-specific remediation.
            e2e_check.remediation = Some(match e2e_check.remediation.take() {
                Some(stage) => format!(
                    "a trace2 check failed above — attribution depends on it. run `autter install --system` first, then: {stage}"
                ),
                None => REMEDIATION_TRACE2_THEN_ATTRIBUTION.to_string(),
            });
        }
        reporter.add(e2e_check);
    } else {
        reporter.add(skipped_check(
            SECTION_E2E,
            "checkpoint round-trip",
            "skipped -- fix the background service check first",
        ));
    }

    // Commits in this repository that were made while capture was down (the
    // service stopped or hung, or trace2 not configured) and so never got a
    // note. Their checkpoints are still on disk; rebuild the notes now.
    reporter.begin(SECTION_E2E, "looking for commits that missed attribution");
    reporter.add(check_stranded_commits());

    // AI agent hooks: would an edit made right now actually checkpoint?
    reporter.begin(SECTION_AGENTS, "checking agent hooks");
    for check in agent_hook_checks() {
        reporter.add(check);
    }
    reporter.begin(SECTION_AGENTS, "checking VS Code");
    if let Some(check) = vscode_native_hooks_check() {
        reporter.add(check);
    }

    // Account & sync: auth state and API reachability. Uploads silently
    // no-op when unauthenticated, so doctor is where a broken login surfaces.
    reporter.begin(SECTION_ACCOUNT, "checking your sign-in");
    let (auth_check, authenticated) = check_auth();
    reporter.add(auth_check);
    reporter.begin(SECTION_ACCOUNT, "reaching the Autter API");
    reporter.add(check_connectivity(authenticated));
    reporter.begin(SECTION_ACCOUNT, "checking your org's data plane");
    reporter.add(check_org_data_plane());
    reporter.begin(SECTION_ACCOUNT, "checking the upload queue");
    reporter.add(check_sync_queue());
    reporter.begin(SECTION_ACCOUNT, "checking usage metrics upload");
    reporter.add(check_usage_metrics());

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
fn check_stranded_commits() -> DoctorCheck {
    use crate::authorship::stranded_commits::{recover_stranded_commits, short_sha};
    let name = "missed commits";
    let Ok(repo) = crate::git::find_repository_in_path(".") else {
        return skipped_check(SECTION_E2E, name, "not inside a git repository");
    };
    let author = repo.effective_author_identity().formatted_or_unknown();
    let recovery = match recover_stranded_commits(&repo, "HEAD", &author) {
        Ok(recovery) => recovery,
        Err(err) => {
            return DoctorCheck {
                section: SECTION_E2E,
                name: name.to_string(),
                status: DoctorStatus::Warning,
                summary: "couldn't check this repository for commits that missed attribution"
                    .to_string(),
                details: vec![err.to_string()],
                remediation: None,
            };
        }
    };
    if recovery.is_empty() {
        return DoctorCheck {
            section: SECTION_E2E,
            name: name.to_string(),
            status: DoctorStatus::Passed,
            summary: "no recent commits in this repository are missing attribution".to_string(),
            details: Vec::new(),
            remediation: None,
        };
    }
    let mut details: Vec<String> = recovery
        .recovered
        .iter()
        .map(|sha| format!("recovered {}", short_sha(sha)))
        .collect();
    details.extend(
        recovery
            .failed
            .iter()
            .map(|(sha, err)| format!("could not recover {}: {}", short_sha(sha), err)),
    );
    if recovery.failed.is_empty() {
        DoctorCheck {
            section: SECTION_E2E,
            name: name.to_string(),
            status: DoctorStatus::Warning,
            summary: format!(
                "{} commit(s) were made while capture was down and had no attribution; doctor rebuilt it from the saved checkpoints",
                recovery.recovered.len()
            ),
            details,
            remediation: None,
        }
    } else {
        DoctorCheck {
            section: SECTION_E2E,
            name: name.to_string(),
            status: DoctorStatus::Failed,
            summary: format!(
                "{} commit(s) made while capture was down still have no attribution",
                recovery.failed.len()
            ),
            details,
            remediation: Some(
                "fix the background service and trace2 checks above, then run `autter doctor` again; the checkpoints are kept until then".to_string(),
            ),
        }
    }
}

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

    // The same path `install-hooks` writes, so a symlinked or cargo-built
    // binary isn't reported as out of date when the hooks are current.
    let binary_path = crate::mdm::utils::resolve_hook_binary_path()
        .or_else(|_| std::env::current_exe())
        .unwrap_or_else(|_| std::path::PathBuf::from("autter"));
    let params = HookInstallerParams { binary_path };
    // Refresh failures only count for this CLI version: an older version's
    // failures say nothing about the hooks this binary would write.
    let refresh_state = crate::mdm::hook_refresh::read_state()
        .filter(|state| state.version == env!("CARGO_PKG_VERSION"));

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
            Ok(result) if !result.hooks_installed && installer.id() == "jetbrains" => checks.push(DoctorCheck {
                section: SECTION_AGENTS,
                name: name.clone(),
                status: DoctorStatus::Failed,
                summary:
                    "detected, but the autter JetBrains plugin is not installed -- edits are not captured"
                        .to_string(),
                details: Vec::new(),
                remediation: Some(
                    "run `autter install-hooks`, then install/enable the `autter` plugin from JetBrains Settings > Plugins and restart the IDE"
                        .to_string(),
                ),
            }),
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
            Ok(result) if result.hooks_up_to_date && installer.id() == "antigravity" => {
                checks.push(DoctorCheck {
                    section: SECTION_AGENTS,
                    name,
                    status: DoctorStatus::Passed,
                    summary: "hooks installed (up to date) -- captured in the Antigravity CLI and 2.0 app; the Antigravity IDE has been reported not to run hooks, see docs/unsupported-agents.md".to_string(),
                    details: Vec::new(),
                    remediation: None,
                })
            }
            Ok(result) if result.hooks_up_to_date => checks.push(DoctorCheck {
                section: SECTION_AGENTS,
                name,
                status: DoctorStatus::Passed,
                summary: "hooks installed (up to date)".to_string(),
                details: Vec::new(),
                remediation: None,
            }),
            Ok(_) => {
                let commands = crate::mdm::hook_refresh::autter_commands_in_files(
                    &installer.hook_config_paths(),
                );
                let refresh_failure = refresh_state
                    .as_ref()
                    .and_then(|s| s.failed.iter().find(|f| f.id == installer.id()));
                checks.push(stale_hooks_check(
                    &name,
                    &commands,
                    &|cmd: &str| crate::mdm::hook_refresh::command_problem(cmd),
                    refresh_failure.map(|f| f.error.as_str()),
                ));
            }
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

    if let Some(check) = claude_shell_capture_check() {
        checks.push(check);
    }

    // Agents autter has no preset for: their edits land as untracked and
    // blame credits the user. A warning, not a failure -- nothing is broken,
    // but attribution for that tool is missing.
    let unsupported = crate::mdm::unsupported_agents::detect_unsupported_agents();
    for agent in &unsupported {
        checks.push(DoctorCheck {
            section: SECTION_AGENTS,
            name: agent.name.to_string(),
            status: DoctorStatus::Warning,
            summary: crate::mdm::unsupported_agents::not_captured_message(agent),
            details: vec![
                agent.evidence.clone(),
                "autter has no integration for this tool yet; edits made with it show as untracked in `autter stats` and as yours in `autter blame`".to_string(),
                "what is missing and what would unblock it: docs/unsupported-agents.md (https://github.com/Autter-dev/autter-cli/blob/main/docs/unsupported-agents.md)".to_string(),
            ],
            remediation: None,
        });
    }

    if checks.len() == unsupported.len() && !not_detected.is_empty() {
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

/// Out-of-date hooks for one agent. **Failed** when a hook command will fail
/// when the agent runs it (the binary it names is gone, or its preset is one
/// this autter doesn't know) or when the automatic refresh after an upgrade
/// could not rewrite them; each failing command is listed exactly. Otherwise
/// the hooks still run and only differ cosmetically: **Warning**.
fn stale_hooks_check(
    name: &str,
    commands: &[String],
    problem: &dyn Fn(&str) -> Option<String>,
    refresh_failure: Option<&str>,
) -> DoctorCheck {
    let remediation = Some(format!(
        "run `autter install-hooks` to refresh them, then restart {name}"
    ));
    let failing: Vec<(String, &String)> = commands
        .iter()
        .filter_map(|cmd| problem(cmd).map(|why| (why, cmd)))
        .collect();
    if let Some((why, _)) = failing.first() {
        let mut details: Vec<String> = failing
            .iter()
            .map(|(why, cmd)| format!("fails because {why}: {cmd}"))
            .collect();
        if let Some(error) = refresh_failure {
            details.push(format!(
                "automatic refresh after the last upgrade failed: {error}"
            ));
        }
        return DoctorCheck {
            section: SECTION_AGENTS,
            name: name.to_string(),
            status: DoctorStatus::Failed,
            summary: format!("hooks out of date and failing every time the agent runs them: {why}"),
            details,
            remediation,
        };
    }
    if let Some(error) = refresh_failure {
        let details = commands
            .iter()
            .map(|cmd| format!("hook command: {cmd}"))
            .collect();
        return DoctorCheck {
            section: SECTION_AGENTS,
            name: name.to_string(),
            status: DoctorStatus::Failed,
            summary: format!("hooks out of date and could not be refreshed: {error}"),
            details,
            remediation,
        };
    }
    DoctorCheck {
        section: SECTION_AGENTS,
        name: name.to_string(),
        status: DoctorStatus::Warning,
        summary: "hooks installed but out of date (they still run)".to_string(),
        details: Vec::new(),
        remediation,
    }
}

/// Claude Code shell-tool capture: hooks installed by older autter versions
/// matched only `Write|Edit|MultiEdit`, so edits Claude made with shell
/// commands were committed as untracked with nothing flagging it.
fn claude_shell_capture_check() -> Option<DoctorCheck> {
    use crate::mdm::agents::{ClaudeShellCapture, claude_shell_capture_status};
    match claude_shell_capture_status() {
        ClaudeShellCapture::NotInstalled => None,
        ClaudeShellCapture::Captured => Some(DoctorCheck {
            section: SECTION_AGENTS,
            name: "Claude Code shell edits".to_string(),
            status: DoctorStatus::Passed,
            summary: "captured (autter hooks run on the Bash tool)".to_string(),
            details: Vec::new(),
            remediation: None,
        }),
        ClaudeShellCapture::NotCaptured { missing_events } => Some(DoctorCheck {
            section: SECTION_AGENTS,
            name: "Claude Code shell edits".to_string(),
            status: DoctorStatus::Warning,
            summary:
                "NOT captured -- edits Claude makes with shell commands are committed as untracked"
                    .to_string(),
            details: vec![format!(
                "the {} hook matcher in Claude Code settings excludes the Bash tool",
                missing_events.join("/")
            )],
            remediation: Some("run `autter install-hooks`, then restart Claude Code".to_string()),
        }),
    }
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
        VsCodeChainOutcome::Complete => {
            let (status, summary) = copilot_capture_verdict(
                crate::commands::checkpoint_agent::hook_heartbeat::read("github-copilot"),
                crate::commands::checkpoint_agent::hook_heartbeat::now(),
            );
            Some(DoctorCheck {
                section: SECTION_AGENTS,
                name,
                status,
                summary,
                details: chain.lines,
                remediation: (status != DoctorStatus::Passed).then(|| {
                    "check the dropped-edit reason above; if it persists, run `autter install-hooks` and restart VS Code".to_string()
                }),
            })
        }
        VsCodeChainOutcome::Incomplete => {
            // Hooks can also load from a workspace `.github/hooks` file, so a
            // recent checkpoint means capture works even if our chain looks
            // incomplete.
            let recent_checkpoint =
                crate::commands::checkpoint_agent::hook_heartbeat::read("github-copilot")
                    .and_then(|heartbeat| heartbeat.last_checkpoint_at)
                    .filter(|at| {
                        crate::commands::checkpoint_agent::hook_heartbeat::now().saturating_sub(*at)
                            < 7 * 86_400
                    });
            Some(DoctorCheck {
                section: SECTION_AGENTS,
                name,
                status: if recent_checkpoint.is_some() {
                    DoctorStatus::Warning
                } else {
                    DoctorStatus::Failed
                },
                summary: if recent_checkpoint.is_some() {
                    "Copilot checkpoints are arriving, but the user-level hook setup is incomplete"
                        .to_string()
                } else {
                    "Copilot agent-mode edits are likely NOT being captured".to_string()
                },
                details: chain.lines,
                remediation: Some("run `autter install-hooks` and restart VS Code".to_string()),
            })
        }
    }
}

/// Turn the Copilot hook heartbeat into a doctor verdict once the hook chain
/// is installed: has a Copilot edit actually been checkpointed, and was the
/// most recent edit dropped?
fn copilot_capture_verdict(
    heartbeat: Option<crate::commands::checkpoint_agent::hook_heartbeat::HookHeartbeat>,
    now: u64,
) -> (DoctorStatus, String) {
    use crate::commands::checkpoint_agent::hook_heartbeat::describe_age;

    let Some(heartbeat) = heartbeat else {
        return (
            DoctorStatus::Passed,
            "Copilot agent hooks are installed; no Copilot agent edit seen yet (make one in agent mode, then re-run doctor)"
                .to_string(),
        );
    };
    let dropped_after_last_checkpoint =
        match (heartbeat.last_error_at, heartbeat.last_checkpoint_at) {
            (Some(error_at), Some(checkpoint_at)) => error_at > checkpoint_at,
            (Some(_), None) => true,
            _ => false,
        };
    if dropped_after_last_checkpoint {
        let reason = heartbeat
            .last_error
            .clone()
            .unwrap_or_else(|| "unknown reason".to_string());
        return (
            DoctorStatus::Warning,
            format!(
                "Copilot hooks run, but the last Copilot edit ({}) was not checkpointed: {}",
                describe_age(
                    heartbeat.last_error_at.unwrap_or(heartbeat.last_run_at),
                    now
                ),
                reason
            ),
        );
    }
    match heartbeat.last_checkpoint_at {
        Some(at) => (
            DoctorStatus::Passed,
            format!(
                "Copilot agent-mode capture is working (last Copilot checkpoint {})",
                describe_age(at, now)
            ),
        ),
        None => (
            DoctorStatus::Passed,
            format!(
                "Copilot agent hooks are running (last {}); no Copilot edit checkpointed yet",
                describe_age(heartbeat.last_run_at, now)
            ),
        ),
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
                status: if Config::get().notes_backend_kind().uses_http() {
                    DoctorStatus::Failed
                } else {
                    DoctorStatus::Warning
                },
                summary: if Config::get().notes_backend_kind().uses_http() {
                    "cloud sync is blocked because stored credentials cannot authenticate"
                        .to_string()
                } else {
                    "stored credentials cannot authenticate; ignored in local mode".to_string()
                },
                details,
                remediation: Some(
                    "run `autter login`, then `autter bg restart` (or ignore in local-only mode)"
                        .to_string(),
                ),
            },
            has_api_key,
        ),
        AuthState::RefreshExpired => (
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: if Config::get().notes_backend_kind().uses_http() {
                    DoctorStatus::Failed
                } else {
                    DoctorStatus::Warning
                },
                summary: if Config::get().notes_backend_kind().uses_http() {
                    "login session expired -- cloud sync is silently disabled until you log in again"
                        .to_string()
                } else {
                    "login session expired; cloud sync is off in local mode".to_string()
                },
                details,
                remediation: Some(
                    "run `autter login` to re-authenticate (ignore this in local-only mode)"
                        .to_string(),
                ),
            },
            has_api_key,
        ),
        AuthState::LoginRefused(reason) => (
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: if Config::get().notes_backend_kind().uses_http() {
                    DoctorStatus::Failed
                } else {
                    DoctorStatus::Warning
                },
                summary: format!("the server refused the saved login: {}", reason),
                details,
                remediation: Some(
                    "run `autter login` to sign in again (ignore this in local-only mode)"
                        .to_string(),
                ),
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

    match client.check_data_plane() {
        Ok(()) => {
            crate::auth::notice::clear_sync_auth_blocked();
            DoctorCheck {
                section: SECTION_ACCOUNT,
                name,
                status: DoctorStatus::Passed,
                summary: "server-side upload API is reachable".to_string(),
                details: Vec::new(),
                remediation: None,
            }
        }
        Err(error) => DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Failed,
            summary: "could not reach the server-side upload API".to_string(),
            details: vec![format!("error: {error}")],
            remediation: Some(
                "check network access to the Autter API, then run `autter bg restart` and `autter doctor`"
                    .to_string(),
            ),
        },
    }
}

fn check_sync_queue() -> DoctorCheck {
    let name = "durable sync queue".to_string();

    // Local mode never uploads anything. Leftover records queued by an earlier
    // connected session (or by older versions) stay on disk and are only
    // uploaded if the user reconnects and agrees to import them, so they must
    // not fail CI / doctor.
    if !Config::fresh().platform_sync_enabled() {
        let pending = crate::auth::notice::pending_sync_counts();
        let held = crate::upload_hold::held_counts();
        let mut details = Vec::new();
        if pending.total() + held.total() > 0 {
            details.push(format!(
                "left over from an earlier connected session or autter version: {} (plus {} held)",
                pending.summary(),
                held.total()
            ));
            details.push(
                "not uploaded in local mode; `autter onboard --connect` asks before importing them"
                    .to_string(),
            );
            details.push("delete them with `autter sync purge --force`".to_string());
        }
        // Signed in but in local mode is the case people mistake for
        // "connected": say it here, with the same verdict `whoami` and
        // `autter status` print (both read `platform_sync_enabled`).
        let signed_in = matches!(collect_auth_status().state, AuthState::LoggedIn);
        return local_mode_sync_check(name, signed_in, details);
    }

    // Notes stay in git notes; cloud upload queues are not checked.
    if !Config::fresh().notes_backend_kind().uses_http() {
        return DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Passed,
            summary: "git-notes backend — cloud upload queues are not checked".to_string(),
            details: vec![crate::auth::notice::pending_sync_counts().summary()],
            remediation: None,
        };
    }

    // Usage metrics have their own check (`check_usage_metrics`): a metrics
    // backlog says nothing about whether attribution reaches the dashboard.
    let pending = crate::auth::notice::pending_sync_counts().attribution_only();
    let mut details = vec![pending.attribution_summary()];
    let held = crate::upload_hold::held_counts();
    if held.total() > 0 {
        details.push(format!(
            "held until you decide (queued before connecting or by an older autter): {}",
            held.summary()
        ));
    }
    // Auth-blocked is reported on its own. The upload-failure stamp is the
    // case `bg status` calls `upload_failing` while this check used to pass.
    if !crate::auth::notice::sync_auth_blocked_recently()
        && let Some(failure) = crate::auth::notice::current_upload_failure()
    {
        details.insert(0, format!("failing queue: {}", failure.queue));
        details.insert(1, format!("error: {}", failure.detail));
        return DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Failed,
            summary: format!("{} upload is failing", failure.queue),
            details,
            remediation: Some(format!(
                "{} upload failed ({}); fix that error, then run `autter bg restart` and `autter doctor`",
                failure.queue, failure.detail
            )),
        };
    }
    if pending.total() == 0 && held.total() > 0 {
        return DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Warning,
            summary: format!(
                "{} records are held and will not upload until you decide",
                held.total()
            ),
            details,
            remediation: Some(format!(
                "upload them with `{}`, or delete them with `{}`",
                crate::upload_hold::IMPORT_COMMAND,
                crate::upload_hold::PURGE_COMMAND
            )),
        };
    }
    if pending.total() == 0 {
        return DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Passed,
            summary: "all local attribution upload queues are empty".to_string(),
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

fn check_usage_metrics() -> DoctorCheck {
    let sync_enabled = Config::fresh().platform_sync_enabled();
    let queued = crate::auth::notice::pending_sync_counts().metrics;
    let status = crate::metrics::sync_state::UsageMetricsStatus::current(queued);
    usage_metrics_check(&status, sync_enabled)
}

/// The usage-metrics verdict. Metrics are product telemetry, not attribution:
/// a failing metrics upload is a warning that names the endpoint and the
/// error, and never says attribution is broken or asks for a restart (the
/// queue is durable and the daemon already retries, in smaller batches when
/// the server is too slow).
fn usage_metrics_check(
    status: &crate::metrics::sync_state::UsageMetricsStatus,
    sync_enabled: bool,
) -> DoctorCheck {
    use crate::metrics::sync_state::{advice, describe_failure};
    let name = "usage metrics upload".to_string();
    let counts = format!(
        "{} events queued, {} uploaded, {}{}",
        status.queued,
        status.uploaded,
        crate::metrics::sync_state::failed_attempts_text(status.failed_attempts),
        if status.dropped > 0 {
            format!(", {} dropped as unacceptable", status.dropped)
        } else {
            String::new()
        }
    );

    if !sync_enabled {
        return DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Passed,
            summary: "local mode — usage metrics are not uploaded".to_string(),
            details: Vec::new(),
            remediation: None,
        };
    }

    if let Some(failure) = status.last_failure.as_ref().filter(|_| status.failing) {
        let mut details = vec![counts, format!("last error: {}", describe_failure(failure))];
        if let Some(next) = status.next_retry_at {
            details.push(format!(
                "next retry: {} with {} events",
                format_unix_timestamp(next),
                status.batch_size
            ));
        } else {
            details.push(format!(
                "next retry: on the next background tick, with {} events",
                status.batch_size
            ));
        }
        details.push(
            "attribution is not affected: authorship notes upload through their own queue"
                .to_string(),
        );
        return DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Warning,
            summary: format!(
                "usage metrics are not uploading: POST {} — {}",
                failure.endpoint, failure.error
            ),
            details,
            remediation: Some(advice(failure, status.batch_size)),
        };
    }

    let summary = if status.queued > 0 {
        format!(
            "{} usage-metrics events queued; uploading in batches of {}",
            status.queued, status.batch_size
        )
    } else {
        "no usage metrics waiting".to_string()
    };
    DoctorCheck {
        section: SECTION_ACCOUNT,
        name,
        status: DoctorStatus::Passed,
        summary,
        details: vec![counts],
        remediation: None,
    }
}

// ===========================================================================
// Plumbing
// ===========================================================================

/// The sync-queue verdict in local mode. Signed in but not connected is a
/// warning: nothing uploads, though the account suggests it would.
fn local_mode_sync_check(name: String, signed_in: bool, details: Vec<String>) -> DoctorCheck {
    if signed_in {
        DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Warning,
            summary: "signed in, but this machine is in local mode — cloud upload is off and nothing reaches your org's dashboard".to_string(),
            details,
            remediation: Some(
                "run `autter onboard --connect` to upload (ignore this if you want local only)"
                    .to_string(),
            ),
        }
    } else {
        DoctorCheck {
            section: SECTION_ACCOUNT,
            name,
            status: DoctorStatus::Passed,
            summary: "local mode — nothing is uploaded to the Autter platform".to_string(),
            details,
            remediation: None,
        }
    }
}

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

/// First failed check that names a fix, otherwise the first warning that does.
/// Checks are already in dependency order (git, hooks, trace2, daemon, agents,
/// account, checkpoint), so the first failure is the one to fix before the rest.
fn next_step_from_checks(checks: &[DoctorCheck]) -> Option<String> {
    for check in checks {
        if check.status == DoctorStatus::Failed
            && let Some(step) = &check.remediation
        {
            return Some(step.clone());
        }
    }
    for check in checks {
        if check.status == DoctorStatus::Warning
            && let Some(step) = &check.remediation
        {
            return Some(step.clone());
        }
    }
    None
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
    /// A person is watching both streams: each check shows a spinner while
    /// it runs and resolves in place, in the terminal's glyphs.
    live: bool,
    current_section: Option<&'static str>,
    running: Option<crate::ui::progress::Spinner>,
    checks: Vec<DoctorCheck>,
}

impl Reporter {
    fn new(json: bool) -> Self {
        let live = !json && std::io::stdout().is_terminal() && crate::ui::caps().tty;
        Self {
            json,
            live,
            current_section: None,
            running: None,
            checks: Vec::new(),
        }
    }

    /// Announce a check that is about to run. Only shows anything live; the
    /// result still arrives through [`Self::add`].
    fn begin(&mut self, section: &'static str, activity: &str) {
        if !self.live {
            return;
        }
        self.enter_section(section);
        if let Some(spinner) = self.running.take() {
            spinner.stop();
        }
        self.running = Some(crate::ui::progress::Spinner::indented(
            &format!("{activity}\u{2026}"),
            2,
        ));
    }

    fn add(&mut self, check: DoctorCheck) {
        if let Some(spinner) = self.running.take() {
            spinner.stop();
        }
        if !self.json {
            self.enter_section(check.section);
            let glyphs = if self.live {
                crate::ui::caps().glyphs()
            } else {
                &crate::ui::theme::UNICODE_GLYPHS
            };
            print_human_check(&check, glyphs);
        }
        self.checks.push(check);
    }

    fn enter_section(&mut self, section: &'static str) {
        if self.current_section != Some(section) {
            println!();
            println!("{}", arg_parser::paint("1", section));
            self.current_section = Some(section);
        }
    }

    fn finish(mut self) -> DoctorOutput {
        if let Some(spinner) = self.running.take() {
            spinner.stop();
        }
        let summary = summarize(&self.checks);
        let next_step = next_step_from_checks(&self.checks);
        let output = DoctorOutput {
            autter_version: version_string(),
            checks: self.checks,
            summary,
            ok: summary.failed == 0,
            next_step,
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
        if let Some(step) = &output.next_step {
            println!("Next step: {}", step);
        }

        output
    }
}

fn print_human_check(check: &DoctorCheck, glyphs: &crate::ui::theme::Glyphs) {
    let (symbol, color) = match check.status {
        DoctorStatus::Passed => (glyphs.ok, "1;32"),
        DoctorStatus::Warning => (glyphs.warn, "1;33"),
        DoctorStatus::Failed => (glyphs.fail, "1;31"),
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

    /// Pretend `/gone/` binaries were deleted by an upgrade.
    fn fake_problem(cmd: &str) -> Option<String> {
        cmd.starts_with("/gone/")
            .then(|| "it runs /gone/autter, which no longer exists".to_string())
    }

    fn stale_json(name: &str, commands: &[&str], refresh_failure: Option<&str>) -> String {
        let commands: Vec<String> = commands.iter().map(|c| c.to_string()).collect();
        let check = stale_hooks_check(name, &commands, &fake_problem, refresh_failure);
        serde_json::to_string_pretty(&check).unwrap()
    }

    #[test]
    fn stale_hooks_that_break_the_agent_fail_with_the_command() {
        insta::assert_snapshot!(stale_json(
            "Google Antigravity",
            &[
                "/gone/autter checkpoint antigravity-pre --hook-input stdin",
                "/gone/autter checkpoint antigravity-post --hook-input stdin",
            ],
            None,
        ));
    }

    #[test]
    fn stale_hooks_that_still_run_are_a_warning() {
        insta::assert_snapshot!(stale_json(
            "Claude Code",
            &["/usr/local/bin/autter checkpoint claude --hook-input stdin"],
            None,
        ));
    }

    #[test]
    fn stale_hooks_that_could_not_be_refreshed_fail() {
        insta::assert_snapshot!(stale_json(
            "Cursor",
            &["/usr/local/bin/autter checkpoint cursor --hook-input stdin"],
            Some("permission denied: ~/.cursor/hooks.json"),
        ));
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
        let next_step = next_step_from_checks(&checks);
        let output = DoctorOutput {
            autter_version: "test".to_string(),
            checks,
            summary,
            ok: summary.failed == 0,
            next_step,
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&output).unwrap()).unwrap();
        assert!(!value["ok"].as_bool().unwrap());
        assert_eq!(value["summary"]["passed"], 1);
        assert_eq!(value["summary"]["failed"], 1);
        assert_eq!(value["checks"][0]["status"], "passed");
        assert_eq!(value["checks"][1]["status"], "failed");
        assert_eq!(value["checks"][1]["remediation"], "do the thing");
        assert_eq!(value["next_step"], "do the thing");
        // Passed checks with no remediation omit the key entirely.
        assert!(value["checks"][0].get("remediation").is_none());
    }

    #[test]
    fn signed_in_local_mode_warns_that_cloud_upload_is_off() {
        let check = local_mode_sync_check("durable sync queue".to_string(), true, Vec::new());
        assert_eq!(check.status, DoctorStatus::Warning);
        assert!(
            check.summary.contains("cloud upload is off"),
            "{}",
            check.summary
        );
        assert!(
            check
                .remediation
                .as_deref()
                .is_some_and(|fix| fix.contains("autter onboard --connect"))
        );

        let check = local_mode_sync_check("durable sync queue".to_string(), false, Vec::new());
        assert_eq!(check.status, DoctorStatus::Passed);
    }

    /// Plain-text form of a check as `autter doctor` prints it.
    fn render_check(check: &DoctorCheck) -> String {
        let status = serde_json::to_value(check.status).unwrap();
        let mut out = format!(
            "[{}] {}: {}\n",
            status.as_str().unwrap(),
            check.name,
            check.summary
        );
        if let Some(fix) = &check.remediation {
            out.push_str(&format!("  fix: {fix}\n"));
        }
        for detail in &check.details {
            out.push_str(&format!("  {detail}\n"));
        }
        out
    }

    /// Usage-metrics status with `queued` events after `failures` in a row,
    /// starting from a full 1,000-event batch, at a fixed clock.
    fn metrics_status_after(
        failures: &[crate::api::metrics::MetricsUploadFailure],
        queued: i64,
    ) -> crate::metrics::sync_state::UsageMetricsStatus {
        use crate::metrics::sync_state::{MetricsSyncState, UsageMetricsStatus};
        let mut state = MetricsSyncState {
            uploaded_total: 1_200,
            ..MetricsSyncState::default()
        };
        let now = 1_762_588_800; // 2025-11-08T08:00:00Z
        for failure in failures {
            let size = (queued as usize).min(state.batch_limit());
            state.record_failure(now, failure, size);
        }
        UsageMetricsStatus::from_state(&state, queued, now)
    }

    #[test]
    fn usage_metrics_timeout_names_the_endpoint_and_is_only_a_warning() {
        let timeout = crate::api::metrics::MetricsUploadFailure::Timeout {
            after_secs: Some(30),
        };
        let check = usage_metrics_check(
            &metrics_status_after(&[timeout.clone(), timeout], 643),
            true,
        );
        assert_eq!(check.status, DoctorStatus::Warning);
        assert!(check.summary.contains("/worker/metrics/upload"));
        assert!(check.summary.contains("timeout after 30s"));
        let fix = check.remediation.as_deref().unwrap();
        assert!(!fix.contains("bg restart"), "{fix}");
        insta::assert_snapshot!(render_check(&check));
    }

    #[test]
    fn usage_metrics_http_413_names_the_status() {
        let failure = crate::api::metrics::MetricsUploadFailure::Http {
            status: 413,
            body: "{\"error\":\"metrics batch too large: send at most 1000 events\"}".to_string(),
        };
        let check = usage_metrics_check(&metrics_status_after(&[failure], 643), true);
        assert_eq!(check.status, DoctorStatus::Warning);
        insta::assert_snapshot!(render_check(&check));
    }

    #[test]
    fn usage_metrics_healthy_and_draining_pass() {
        let check = usage_metrics_check(&metrics_status_after(&[], 0), true);
        assert_eq!(check.status, DoctorStatus::Passed);
        insta::assert_snapshot!("usage_metrics_healthy", render_check(&check));

        let check = usage_metrics_check(&metrics_status_after(&[], 120), true);
        assert_eq!(check.status, DoctorStatus::Passed);
        insta::assert_snapshot!("usage_metrics_draining", render_check(&check));
    }

    #[test]
    fn usage_metrics_failure_with_an_empty_queue_is_not_failing() {
        // A purge or drain leaves no events; an old error is not news.
        let timeout = crate::api::metrics::MetricsUploadFailure::Timeout {
            after_secs: Some(30),
        };
        let check = usage_metrics_check(&metrics_status_after(&[timeout], 0), true);
        assert_eq!(check.status, DoctorStatus::Passed);
    }

    #[test]
    fn usage_metrics_in_local_mode_pass() {
        let timeout = crate::api::metrics::MetricsUploadFailure::Timeout {
            after_secs: Some(30),
        };
        let check = usage_metrics_check(&metrics_status_after(&[timeout], 643), false);
        assert_eq!(check.status, DoctorStatus::Passed);
        insta::assert_snapshot!(render_check(&check));
    }

    #[test]
    fn trace2_remediations_never_send_people_back_to_onboard() {
        assert!(!REMEDIATION_TRACE2_THEN_ATTRIBUTION.contains("onboard"));
    }

    #[test]
    fn next_step_prefers_the_first_failure_over_a_warning() {
        let mut warning = make_check(DoctorStatus::Warning);
        warning.remediation = Some("review the warning".to_string());
        let mut failed = make_check(DoctorStatus::Failed);
        failed.remediation = Some("run autter install --system".to_string());
        let checks = vec![warning, failed];
        assert_eq!(
            next_step_from_checks(&checks).as_deref(),
            Some("run autter install --system")
        );
        assert_eq!(
            next_step_from_checks(&[make_check(DoctorStatus::Passed)]),
            None
        );
    }

    fn heartbeat(
        last_outcome: crate::commands::checkpoint_agent::hook_heartbeat::HookOutcome,
        last_checkpoint_at: Option<u64>,
        last_error_at: Option<u64>,
        last_error: Option<&str>,
    ) -> crate::commands::checkpoint_agent::hook_heartbeat::HookHeartbeat {
        crate::commands::checkpoint_agent::hook_heartbeat::HookHeartbeat {
            last_run_at: 1_000,
            last_outcome,
            last_detail: None,
            last_checkpoint_at,
            last_error_at,
            last_error: last_error.map(str::to_string),
        }
    }

    #[test]
    fn copilot_verdict_without_activity_passes_with_a_hint() {
        let (status, summary) = copilot_capture_verdict(None, 2_000);
        assert_eq!(status, DoctorStatus::Passed);
        assert!(
            summary.contains("no Copilot agent edit seen yet"),
            "{summary}"
        );
    }

    #[test]
    fn copilot_verdict_reports_the_last_checkpoint() {
        use crate::commands::checkpoint_agent::hook_heartbeat::HookOutcome;
        let (status, summary) = copilot_capture_verdict(
            Some(heartbeat(
                HookOutcome::Skipped,
                Some(1_000 - 300),
                None,
                None,
            )),
            1_000,
        );
        assert_eq!(status, DoctorStatus::Passed);
        assert!(summary.contains("capture is working"), "{summary}");
        assert!(summary.contains("5 min ago"), "{summary}");
    }

    #[test]
    fn copilot_verdict_warns_when_the_latest_edit_was_dropped() {
        use crate::commands::checkpoint_agent::hook_heartbeat::HookOutcome;
        let (status, summary) = copilot_capture_verdict(
            Some(heartbeat(
                HookOutcome::Error,
                Some(500),
                Some(900),
                Some("cwd not found in hook_input"),
            )),
            1_000,
        );
        assert_eq!(status, DoctorStatus::Warning);
        assert!(summary.contains("cwd not found in hook_input"), "{summary}");

        // A dropped edit followed by a good checkpoint is fine again.
        let (status, _) = copilot_capture_verdict(
            Some(heartbeat(
                HookOutcome::Checkpoint,
                Some(950),
                Some(900),
                Some("x"),
            )),
            1_000,
        );
        assert_eq!(status, DoctorStatus::Passed);
    }
}
