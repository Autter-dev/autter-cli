use crate::authorship::ignore::effective_ignore_patterns;
use crate::authorship::internal_db::InternalDatabase;
use crate::authorship::range_authorship;
use crate::authorship::stats::{CaptureStatus, commit_stats_report, print_commit_stats_report};
use crate::commands;
use crate::config;
use crate::daemon::ControlRequest;
use crate::error::AutterError;
use crate::git::find_repository;
use crate::git::find_repository_in_path;
use crate::git::repository::{CommitRange, Repository};
use crate::git::sync_authorship::{NotesExistence, fetch_authorship_notes, push_authorship_notes};
use crate::observability::log_message;
use crate::utils::is_interactive_terminal;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::IsTerminal;
use std::io::Read;

pub fn handle_autter(args: &[String]) {
    let perf_entry =
        if std::env::var("AUTTER_DEBUG_PERFORMANCE").is_ok_and(|v| !v.is_empty() && v != "0") {
            Some(std::time::Instant::now())
        } else {
            None
        };

    if args.is_empty() {
        // A person at a terminal gets their setup at a glance; `autter help`
        // still lists every command.
        if crate::ui::caps().tty {
            commands::home::print_home();
            return;
        }
        print_help();
        return;
    }

    // Extract leading global flags (e.g. `autter -C <path> --json status`) that
    // appear before the subcommand, matching git's `git -C <path> <cmd>` model.
    // The git proxy path is separate and never reaches here; for unmigrated
    // subcommands any non-leading flags simply pass through untouched in `rest`.
    let leading = match commands::arg_parser::pre_parse(
        args,
        commands::arg_parser::ScanMode::LeadingOnly,
        true,
    ) {
        Ok(p) => p,
        Err(e) => {
            if invoked_by_agent_hook(args) {
                exit_stale_hook(&e.to_string());
            }
            // The leading flags could not be parsed, so `--json` among them
            // was never recorded: look for it, so the error is JSON too.
            commands::arg_parser::merge_json_flag_from(args);
            commands::output::fail(commands::output::ErrorKind::Usage, format!("error: {}", e));
        }
    };
    commands::arg_parser::merge_global_flags(&leading.flags);
    // A reporting command takes `--json` after its name as well. Record it
    // here, before anything the dispatcher itself can fail on (`-C`, the
    // background service), so those failures print a JSON document too.
    if let Some(command) = leading.rest.first()
        && is_reporting_command(command)
    {
        commands::arg_parser::merge_json_flag_from(&leading.rest[1..]);
    }
    if let Some(path) = leading.flags.change_dir.as_deref()
        && let Err(e) = std::env::set_current_dir(path)
    {
        commands::output::fail(
            commands::output::ErrorKind::Runtime,
            format!("error: cannot change to '{}': {}", path, e),
        );
    }

    // `-h`/`--help` before any subcommand: overview, or per-command help when a
    // command name follows (`autter --help status`).
    if leading.flags.help {
        match leading.rest.first() {
            Some(cmd) => commands::arg_parser::print_command_help(cmd),
            None => commands::arg_parser::print_overview(),
        }
        std::process::exit(0);
    }

    // Re-bind `args` to the post-leading-globals view: `args[0]` is now the
    // subcommand regardless of any leading global flags.
    let args: &[String] = &leading.rest;
    if args.is_empty() {
        commands::arg_parser::print_overview();
        std::process::exit(0);
    }

    // Record the subcommand so any later panic report can name it.
    crate::observability::set_current_command(args[0].as_str());

    // Agents that parse a hook's stdout get their no-op reply before anything
    // else can fail (the background service check below, argument parsing,
    // a panic), so every exit path leaves the agent unblocked.
    if args[0] == "checkpoint" {
        answer_checkpoint_hook_early(&args[1..]);
    }

    // `-h`/`--help` anywhere after the subcommand (`autter uninstall --help`)
    // prints that command's help and exits 0 here, before the background
    // service is contacted and before the handler runs, so asking for help can
    // never install, uninstall, log out, upload or open anything. Handlers
    // listed in `handles_own_help` print richer per-subcommand help and check
    // for it before any work; they still skip the background service below.
    let help_requested = commands::arg_parser::wants_help(&args[1..]);
    if help_requested && !commands::arg_parser::handles_own_help(args[0].as_str()) {
        commands::arg_parser::print_command_help(args[0].as_str());
        std::process::exit(0);
    }

    // Initialize the global telemetry handle so that observability and CAS
    // events are routed over the control socket instead of being written to
    // per-PID log files.
    //
    // Skip for commands that must work without a running background service
    // (help, version, config, d management, debug, doctor, upgrade) so users
    // can always diagnose and recover from a broken state.
    let works_without_daemon = matches!(
        args[0].as_str(),
        "help"
            | "--help"
            | "-h"
            | "version"
            | "--version"
            | "-v"
            | "config"
            | "bg"
            | "d"
            | "daemon"
            | "debug"
            | "doctor"
            | "sync"
            | "upgrade"
            | "verify"
            | "install-hooks"
            | "install"
            | "uninstall-hooks"
            | "uninstall"
            | "telemetry"
            | "login"
            | "logout"
            | "whoami"
            | "ask"
            | "logs"
            | "threads"
            | "badge"
            | "memory"
            | "context"
            | "mcp"
    );
    // A help request never needs the background service: `stats --help` used
    // to fail when it was unreachable while `--help stats` worked.
    let needs_daemon = !help_requested && !works_without_daemon;
    if needs_daemon {
        use crate::daemon::telemetry_handle::{
            DaemonTelemetryInitResult, init_daemon_telemetry_handle_with,
        };
        // Before onboarding, plain commands use a daemon that is already
        // running but never start one: the background service is a system
        // integration that needs the user's consent. Agent checkpoints come
        // from hooks the user already installed, so they may still start it.
        let allow_spawn =
            args[0].as_str() == "checkpoint" || crate::config::collection_consented_now();
        match init_daemon_telemetry_handle_with(allow_spawn) {
            DaemonTelemetryInitResult::Connected | DaemonTelemetryInitResult::Skipped => {}
            // A command that does not exist fails as one whether or not the
            // service is up (`autter stat --json` is a usage error with its
            // one JSON document), so it is left to the dispatcher below.
            DaemonTelemetryInitResult::Failed(_) if !is_autter_command(args[0].as_str()) => {}
            DaemonTelemetryInitResult::Failed(err) => {
                eprintln!(
                    "error: failed to connect to autter background service: {}",
                    err
                );
                commands::suggest_autter_doctor();
                // Agent hooks never fail the agent: right after an upgrade the
                // old service may be stopping while the new one starts. That
                // holds for a hook subcommand this autter does not have, too.
                if matches!(args[0].as_str(), "checkpoint" | "memory-hook") {
                    std::process::exit(0);
                }
                if has_hook_input_arg(args) {
                    answer_hook_noop();
                    std::process::exit(0);
                }
                // Exit 1. With `--json` the caller still gets its one
                // document; the message and the hint are on stderr already.
                commands::output::fail_reported(
                    commands::output::ErrorKind::ServiceUnreachable,
                    format!("failed to connect to autter background service: {}", err),
                );
            }
        }
    }

    // Start DB warmup early for commands that need database access
    if args[0].as_str() == "show-prompt" {
        InternalDatabase::warmup();
    }

    match args[0].as_str() {
        "ask" | "logs" | "threads" => commands::ask::handle(&args[0], &args[1..]),
        "memory" => commands::memory::handle(&args[1..]),
        "context" => commands::context::handle(&args[1..]),
        "mcp" => commands::mcp::handle(&args[1..]),
        "help" | "--help" | "-h" => {
            match args.get(1) {
                Some(cmd) => commands::arg_parser::print_command_help(cmd),
                None => commands::arg_parser::print_overview(),
            }
            std::process::exit(0);
        }
        "version" => {
            // Human-friendly release info. Keep `--version`/`-v` bare below so
            // scripts (e.g. install.sh) can parse the raw version string.
            let version = env!("CARGO_PKG_VERSION");
            if cfg!(debug_assertions) {
                println!("autter {version} (debug build)");
            } else {
                println!("autter {version}");
            }
            std::process::exit(0);
        }
        "--version" | "-v" => {
            if cfg!(debug_assertions) {
                println!("{} (debug)", env!("CARGO_PKG_VERSION"));
            } else {
                println!(env!("CARGO_PKG_VERSION"));
            }
            std::process::exit(0);
        }
        "config" => {
            commands::config::handle_config(&args[1..]);
            if is_interactive_terminal() {
                log_message("config", "info", None)
            }
        }
        "debug" => {
            commands::debug::handle_debug(&args[1..]);
        }
        "doctor" => {
            commands::doctor::handle_doctor(&args[1..]);
        }
        "sync" => {
            commands::sync::handle_sync(&args[1..]);
        }
        "bg" | "d" | "daemon" => {
            commands::daemon::handle_daemon(&args[1..]);
        }
        "stats" => {
            if is_interactive_terminal() {
                log_message("stats", "info", None)
            }
            handle_stats(&args[1..]);
        }
        "badge" => {
            commands::badge::handle_badge(&args[1..]);
        }
        "wrapped" => {
            if is_interactive_terminal() {
                log_message("wrapped", "info", None)
            }
            commands::wrapped::handle_wrapped(&args[1..]);
        }
        "file-changes" => {
            commands::file_changes::handle_file_changes(&args[1..]);
        }
        "status" => {
            commands::status::handle_status(&args[1..]);
        }
        "show" => {
            commands::show::handle_show(&args[1..]);
        }
        "memory-hook" => {
            commands::memory_hook::handle_memory_hook(&args[1..]);
        }
        "checkpoint" => {
            if let Some(t) = perf_entry {
                eprintln!(
                    "[perf] checkpoint: entry_overhead={:.1}ms (binary startup + clap + dispatch)",
                    t.elapsed().as_secs_f64() * 1000.0
                );
            }
            if invoked_by_agent_hook(args) {
                // A bug in autter must not block the agent either: the panic
                // is still reported (panic hook), then the hook exits 0.
                let checkpoint_args = args[1..].to_vec();
                if std::panic::catch_unwind(move || handle_checkpoint(&checkpoint_args)).is_err() {
                    eprintln!("autter checkpoint failed internally; the agent was not blocked");
                    std::process::exit(0);
                }
            } else {
                handle_checkpoint(&args[1..]);
            }
        }
        "log" => {
            let status = commands::log::handle_log(&args[1..]);
            if is_interactive_terminal() {
                log_message("log", "info", None)
            }
            exit_with_log_status(status);
        }
        "blame" => {
            handle_ai_blame(&args[1..]);
            if is_interactive_terminal() {
                log_message("blame", "info", None)
            }
        }
        "diff" => {
            handle_ai_diff(&args[1..]);
            if is_interactive_terminal() {
                log_message("diff", "info", None)
            }
        }
        "git-path" => {
            let config = config::Config::get();
            println!("{}", config.git_cmd());
            std::process::exit(0);
        }
        "install-hooks" | "install" => match commands::install_hooks::run(&args[1..]) {
            Ok(statuses) => {
                if let Ok(statuses_value) = serde_json::to_value(&statuses) {
                    log_message("install-hooks", "info", Some(statuses_value));
                }
            }
            Err(AutterError::Usage(message)) => {
                eprintln!("{message}");
                std::process::exit(commands::EXIT_USAGE_ERROR);
            }
            Err(e) => {
                eprintln!("Install hooks failed: {}", e);
                commands::suggest_autter_doctor();
                std::process::exit(1);
            }
        },
        "uninstall-hooks" => match commands::install_hooks::run_uninstall(&args[1..]) {
            Ok(statuses) => {
                if let Ok(statuses_value) = serde_json::to_value(&statuses) {
                    log_message("uninstall-hooks", "info", Some(statuses_value));
                }
            }
            Err(AutterError::Usage(message)) => {
                eprintln!("{message}");
                std::process::exit(commands::EXIT_USAGE_ERROR);
            }
            Err(e) => {
                eprintln!("Uninstall hooks failed: {}", e);
                commands::suggest_autter_doctor();
                std::process::exit(1);
            }
        },
        "uninstall" => match commands::install_hooks::run_full_uninstall(&args[1..]) {
            // run_full_uninstall prints a line for each thing it actually removed.
            Ok(statuses) => {
                if let Ok(statuses_value) = serde_json::to_value(&statuses) {
                    log_message("uninstall", "info", Some(statuses_value));
                }
            }
            Err(AutterError::Usage(message)) => {
                eprintln!("{message}");
                std::process::exit(commands::EXIT_USAGE_ERROR);
            }
            Err(e) => {
                eprintln!("Uninstall failed: {}", e);
                commands::suggest_autter_doctor();
                std::process::exit(1);
            }
        },
        "git-hooks" => {
            handle_git_hooks(&args[1..]);
        }
        "squash-authorship" => {
            commands::squash_authorship::handle_squash_authorship(&args[1..]);
        }
        "ci" => {
            commands::ci_handlers::handle_ci(&args[1..]);
        }
        "upgrade" => {
            commands::upgrade::run_with_args(&args[1..]);
        }
        "verify" => {
            commands::verify::handle_verify(&args[1..]);
        }
        "flush-metrics-db" => {
            commands::flush_metrics_db::handle_flush_metrics_db(&args[1..]);
        }
        "onboard" | "onboarding" | "setup" => {
            commands::onboard::handle_onboard(&args[1..]);
        }
        "telemetry" => {
            commands::telemetry::handle_telemetry(&args[1..]);
        }
        "login" => {
            commands::login::handle_login(&args[1..]);
        }
        "logout" => {
            commands::logout::handle_logout(&args[1..]);
        }
        "whoami" => {
            commands::whoami::handle_whoami(&args[1..]);
        }
        "exchange-nonce" => {
            commands::exchange_nonce::handle_exchange_nonce(&args[1..]);
        }
        "dash" | "dashboard" | "personal-dashboard" | "personal_dashboard" => {
            commands::personal_dashboard::handle_personal_dashboard(&args[1..]);
        }
        "show-prompt" => {
            commands::show_prompt::handle_show_prompt(&args[1..]);
        }
        "fetch-notes" => {
            commands::fetch_notes::handle_fetch_notes(&args[1..]);
        }
        "effective-ignore-patterns" => {
            handle_effective_ignore_patterns_internal(&args[1..]);
        }
        "blame-analysis" => {
            handle_blame_analysis_internal(&args[1..]);
        }
        "fetch-authorship-notes" | "fetch_authorship_notes" => {
            handle_fetch_authorship_notes_internal(&args[1..]);
        }
        "push-authorship-notes" | "push_authorship_notes" => {
            handle_push_authorship_notes_internal(&args[1..]);
        }
        "notes" => {
            handle_notes_subcommand(&args[1..]);
        }
        _ => {
            // A hook written by another autter version can name a subcommand
            // this one does not have (a downgrade, or a renamed command).
            // Exit 2 is the code Claude Code treats as "block the tool call",
            // so a hook gets the stale-hook answer instead: `{}` and exit 0.
            if has_hook_input_arg(args) {
                answer_hook_noop();
                exit_stale_hook(&format!("unknown command '{}'", args[0]));
            }
            // `autter stat --json` (a typo) still answers with one document.
            commands::arg_parser::merge_json_flag_from(&args[1..]);
            commands::output::fail(
                commands::output::ErrorKind::Usage,
                format!("Unknown autter command: {}", args[0]),
            );
        }
    }
}

/// Every command the dispatcher in [`handle_autter`] has an arm for. Keep the
/// two in step: a command missing here is reported as unknown when the
/// background service cannot be reached.
const AUTTER_COMMANDS: &[&str] = &[
    "--help",
    "--version",
    "-h",
    "-v",
    "ask",
    "badge",
    "bg",
    "blame",
    "blame-analysis",
    "checkpoint",
    "ci",
    "config",
    "context",
    "d",
    "daemon",
    "dash",
    "dashboard",
    "debug",
    "diff",
    "doctor",
    "effective-ignore-patterns",
    "exchange-nonce",
    "fetch-authorship-notes",
    "fetch-notes",
    "fetch_authorship_notes",
    "file-changes",
    "flush-metrics-db",
    "git-hooks",
    "git-path",
    "help",
    "install",
    "install-hooks",
    "log",
    "login",
    "logout",
    "logs",
    "mcp",
    "memory",
    "memory-hook",
    "notes",
    "onboard",
    "onboarding",
    "personal-dashboard",
    "personal_dashboard",
    "push-authorship-notes",
    "push_authorship_notes",
    "setup",
    "show",
    "show-prompt",
    "squash-authorship",
    "stats",
    "status",
    "sync",
    "telemetry",
    "threads",
    "uninstall",
    "uninstall-hooks",
    "upgrade",
    "verify",
    "version",
    "whoami",
    "wrapped",
];

fn is_autter_command(command: &str) -> bool {
    AUTTER_COMMANDS.contains(&command)
}

/// The commands whose `--json` output is a documented contract
/// (docs/scripting.md). `sync` is here for `sync status`.
fn is_reporting_command(command: &str) -> bool {
    matches!(
        command,
        "stats" | "status" | "log" | "show" | "blame" | "diff" | "whoami" | "doctor" | "sync"
    )
}

/// Dispatch `autter notes <subcommand>` commands.
fn handle_notes_subcommand(args: &[String]) {
    let subcommand = args.first().map(|s| s.as_str()).unwrap_or("--help");
    match subcommand {
        "migrate" => {
            commands::notes_migrate::handle_notes_migrate(&args[1..]);
        }
        "backfill" => {
            commands::notes_backfill::handle_notes_backfill(&args[1..]);
        }
        "--help" | "-h" | "help" => {
            eprintln!("autter notes - Notes backend management commands");
            eprintln!();
            eprintln!("Usage: autter notes <subcommand> [options]");
            eprintln!();
            eprintln!("Subcommands:");
            eprintln!("  migrate    Bulk-upload existing git notes to your org's database");
            eprintln!(
                "  backfill   Write notes stored only in the local notes database into refs/notes/ai"
            );
            eprintln!();
            eprintln!("Run 'autter notes <subcommand> --help' for details.");
        }
        other => {
            eprintln!("Unknown autter notes subcommand: {}", other);
            eprintln!("Run 'autter notes --help' for usage.");
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    }
}

fn print_help() {
    commands::arg_parser::print_overview();
    std::process::exit(0);
}

/// Run a checkpoint.
///
/// Unlike other user-invoked subcommands, `checkpoint` deliberately exits
/// [`crate::commands::EXIT_SUCCESS`] (`0`) on *every* failure path — bad
/// `--hook-input`, an unknown preset, empty stdin, an unreachable background
/// worker, an excluded repository, etc. This is intentional: `checkpoint` is
/// invoked from inside AI-agent editor hooks (and our own git proxy), where a
/// non-zero exit would surface as an error in the user's editor or break the
/// agent's edit flow. Attribution data is best-effort, so when we cannot record
/// it we degrade silently rather than failing the host process. Do NOT change
/// these to `1`/`2`; see `crate::commands` for the project-wide exit-code
/// policy that governs the *other* (user-facing) subcommands.
fn handle_checkpoint(args: &[String]) {
    // Exit-code policy for `autter checkpoint`:
    //   * Hook invocations (agent presets, IDE extensions, anything passing
    //     `--hook-input`; see `checkpoint_runs_in_hook`) exit 0 on operational
    //     failures -- the agent's edit flow must NEVER break -- and say why on
    //     stderr.
    //   * Direct runs (`autter checkpoint`, `human`, `mock_ai`,
    //     `mock_known_human` typed by a person or a script) exit non-zero when
    //     the checkpoint can't be captured: not in a repository, the background
    //     service unreachable, or git not set up to report the commit. A script
    //     must be able to tell "recorded" from "lost".
    //   * Usage / argument errors from a person or script (unknown preset with
    //     no hook payload) exit non-zero, so they can detect the mistake.
    //   * Usage / argument errors from an agent hook (anything carrying
    //     `--hook-input`, or AUTTER_CHECKPOINT_HOOK set) mean the hook config
    //     was written by another autter version: exit 0 with the agent's
    //     no-op reply and "autter hook is out of date" on stderr
    //     (`exit_stale_hook`). The agent must never be blocked by autter.
    let perf = std::env::var("AUTTER_DEBUG_PERFORMANCE").is_ok_and(|v| !v.is_empty() && v != "0");
    let t0 = std::time::Instant::now();

    // LeadingOnly: the preset name is a positional, so global-flag scanning
    // stops at it and never touches `--hook-input` or pathspecs that follow.
    let pp = match commands::arg_parser::pre_parse(
        args,
        commands::arg_parser::ScanMode::LeadingOnly,
        false,
    ) {
        Ok(p) => p,
        Err(e) => {
            if invoked_by_agent_hook(args) {
                exit_stale_hook(&e.to_string());
            }
            eprintln!("error: {}", e);
            std::process::exit(0);
        }
    };
    if pp.flags.help {
        commands::arg_parser::print_command_help("checkpoint");
        return;
    }
    commands::arg_parser::merge_global_flags(&pp.flags);
    let from_agent_hook = invoked_by_agent_hook(args);
    let args = &pp.rest;

    let mut hook_input = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--hook-input" => {
                if i + 1 < args.len() {
                    hook_input = Some(strip_utf8_bom(args[i + 1].clone()));
                    if hook_input.as_ref().unwrap() == "stdin" {
                        let mut stdin = std::io::stdin();
                        let mut buffer = String::new();
                        if let Err(e) = stdin.read_to_string(&mut buffer) {
                            // Operational (transient IO) condition: don't break the agent.
                            eprintln!("Failed to read stdin for hook input: {}", e);
                            std::process::exit(0);
                        }
                        if buffer.trim().is_empty() {
                            // Operational: nothing to checkpoint -- valid, just a no-op.
                            eprintln!("No hook input provided (via --hook-input or stdin).");
                            std::process::exit(0);
                        }
                        hook_input = Some(strip_utf8_bom(buffer));
                    } else if hook_input.as_ref().unwrap().trim().is_empty() {
                        // The hook command lost its payload argument: a hook
                        // config autter no longer writes. Never block the agent.
                        report_checkpoint_usage_error("--hook-input requires a value");
                        exit_stale_hook("--hook-input has an empty value");
                    }
                    i += 2;
                } else {
                    report_checkpoint_usage_error(
                        "--hook-input requires a value or 'stdin' to read from stdin",
                    );
                    exit_stale_hook("--hook-input has no value");
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    if perf {
        eprintln!(
            "[perf] checkpoint: arg_parse={:.1}ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }

    let (preset_name, file_args): (&str, &[String]) = if args.is_empty() {
        ("human", &[])
    } else if args[0] == "--" {
        ("human", &args[1..])
    } else if crate::commands::checkpoint_agent::presets::resolve_preset(args[0].as_str()).is_err()
    {
        report_checkpoint_usage_error(&format!("unknown checkpoint preset '{}'", args[0]));
        if from_agent_hook {
            // A hook written by an autter that used another preset name.
            exit_stale_hook(&format!("unknown checkpoint preset '{}'", args[0]));
        }
        // Usage error: a person or script named a preset that doesn't exist.
        eprintln!("Error: unknown checkpoint preset '{}'", args[0]);
        eprintln!("Usage: autter checkpoint <preset> [--hook-input <json|stdin>] [files...]");
        std::process::exit(1);
    } else {
        (args[0].as_str(), &args[1..])
    };

    let hook_mode = checkpoint_runs_in_hook(
        preset_name,
        hook_input.is_some(),
        env_flag_set("AUTTER_CHECKPOINT_HOOK"),
    );

    let effective_hook_input =
        hook_input.unwrap_or_else(|| synthesize_hook_input_from_cli_args(preset_name, file_args));

    if perf {
        eprintln!(
            "[perf] checkpoint: synth_hook_input={:.1}ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }

    let t_orchestrator = std::time::Instant::now();
    let requests = match crate::commands::checkpoint_agent::orchestrator::execute_preset_checkpoint(
        preset_name,
        &effective_hook_input,
    ) {
        Ok(r) => r,
        Err(e) => {
            let message = e.to_string();
            crate::commands::checkpoint_agent::hook_heartbeat::record(
                preset_name,
                crate::commands::checkpoint_agent::hook_heartbeat::outcome_for_preset_error(
                    &message,
                ),
                Some(&message),
            );
            let fix = if message.contains("No git repository found") {
                "run it inside a git repository, or pass files that are in one"
            } else {
                "check the file paths, then run `autter doctor`"
            };
            exit_checkpoint_not_captured(
                hook_mode,
                &format!("{} preset error: {}", preset_name, e),
                fix,
            );
        }
    };

    if perf {
        eprintln!(
            "[perf] checkpoint: orchestrator={:.1}ms (requests={}, files={})",
            t_orchestrator.elapsed().as_secs_f64() * 1000.0,
            requests.len(),
            requests.iter().map(|r| r.files.len()).sum::<usize>(),
        );
    }

    if requests.is_empty() {
        // Nothing changed is a valid no-op. Outside any repository, though, a
        // direct run silently did nothing: say so.
        if !hook_mode && !cwd_is_in_git_repository() {
            exit_checkpoint_not_captured(
                false,
                "not in a git repository, so nothing was checkpointed",
                "run `autter checkpoint` inside a git repository, or pass files that are in one",
            );
        }
        std::process::exit(0);
    }

    for request in &requests {
        for file in &request.files {
            if !file.path.is_absolute() {
                exit_checkpoint_not_captured(
                    hook_mode,
                    &format!("file path must be absolute: {}", file.path.display()),
                    "pass absolute paths, or paths relative to the current directory",
                );
            }
        }
    }

    // Check repository allowlist before sending to daemon.
    // Skip entirely when no allow/exclude filters are configured (common case)
    // to avoid spawning a `git remote -v` subprocess.
    let t_allowlist = std::time::Instant::now();
    {
        let config = config::Config::get();
        if config.has_repository_filters() {
            let mut checked_repos = std::collections::HashSet::new();
            for request in &requests {
                // A session-activity request has no files; its repository is in
                // its metadata. Outside any repository there is nothing to check
                // against the filters, so it is skipped rather than let through.
                let session_repo =
                    if crate::commands::checkpoint_agent::orchestrator::is_session_activity(request)
                    {
                        match crate::commands::checkpoint_agent::orchestrator::session_activity_repo(
                            request,
                        ) {
                            Some(repo) => Some(repo),
                            None => {
                                eprintln!(
                                    "Skipping session activity outside a repository because repository filters are configured"
                                );
                                std::process::exit(0);
                            }
                        }
                    } else {
                        None
                    };
                let repo_dirs = request
                    .files
                    .iter()
                    .map(|file| file.repo_work_dir.clone())
                    .chain(session_repo);
                for repo_work_dir in repo_dirs {
                    if checked_repos.insert(repo_work_dir.clone())
                        && let Ok(repo) =
                            crate::git::repository::discover_repository_in_path_no_git_exec(
                                &repo_work_dir,
                            )
                        && !config.is_allowed_repository(&Some(repo))
                    {
                        eprintln!(
                            "Skipping checkpoint because repository is excluded or not in allow_repositories list"
                        );
                        std::process::exit(0);
                    }
                }
            }
        }
    }

    if perf {
        eprintln!(
            "[perf] checkpoint: allowlist={:.1}ms",
            t_allowlist.elapsed().as_secs_f64() * 1000.0
        );
    }

    let t_daemon_config = std::time::Instant::now();
    let daemon_config =
        crate::daemon::DaemonConfig::from_env_or_default_paths().map_err(|e| e.to_string());

    let config = match daemon_config {
        Ok(c) => c,
        Err(e) => {
            exit_checkpoint_not_captured(
                hook_mode,
                &format!("Background worker unavailable: {}", e),
                "check that your home directory is set and writable, then run `autter doctor`",
            );
        }
    };

    if perf {
        eprintln!(
            "[perf] checkpoint: daemon_config={:.1}ms",
            t_daemon_config.elapsed().as_secs_f64() * 1000.0
        );
    }

    // How long to wait for a freshly-spawned daemon to accept connections when a
    // send fails. Paid only when the daemon is down — the exact case that used
    // to silently drop a whole session's checkpoints — so a one-off spawn cost
    // beats losing the data.
    const CHECKPOINT_DAEMON_SPAWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    // The service acknowledges a checkpoint with a receipt as soon as it has
    // read it, before processing it: about a millisecond on a healthy
    // service, however busy it is with other work. Without a receipt the
    // checkpoint cannot be assumed recorded (the connection may sit queued
    // behind others, or the service may be stuck), so the send is retried
    // once and then reported.
    //
    // A hook runs before and after every edit an agent makes, so it waits
    // for nothing but the receipt, and not for long: 200 ms is two orders of
    // magnitude above a healthy receipt, and the agent only ever waits that
    // long when something is wrong. A direct run waits as long as any other
    // control request does for its answer, and then up to as long again for
    // the service to say whether it could record the checkpoint.
    const HOOK_CHECKPOINT_RECEIPT_WAIT: crate::daemon::CheckpointReceiptWait =
        crate::daemon::CheckpointReceiptWait {
            receipt: std::time::Duration::from_millis(200),
            outcome: std::time::Duration::ZERO,
        };
    const DIRECT_CHECKPOINT_RECEIPT_WAIT: crate::daemon::CheckpointReceiptWait =
        crate::daemon::CheckpointReceiptWait {
            receipt: std::time::Duration::from_secs(2),
            outcome: std::time::Duration::from_secs(2),
        };
    let receipt_wait = if hook_mode {
        HOOK_CHECKPOINT_RECEIPT_WAIT
    } else {
        DIRECT_CHECKPOINT_RECEIPT_WAIT
    };
    // Before a resend. Longer than the 50 ms the service backs off after a
    // connection it could not serve, so the resend is not left queued behind
    // that pause.
    const CHECKPOINT_RESEND_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

    let mut sent_count = 0u64;
    // Session-activity requests wake the transcript reader and record no
    // checkpoint, so they are counted apart from checkpoints.
    let mut activity_count = 0u64;
    let mut active_socket = config.control_socket_path.clone();
    let mut tried_spawn = false;
    for request in requests {
        let t_send = std::time::Instant::now();
        let is_activity =
            crate::commands::checkpoint_agent::orchestrator::is_session_activity(&request);
        let control_request = ControlRequest::CheckpointRun {
            request: Box::new(request),
            receipt: true,
        };
        let mut send_result = crate::daemon::send_checkpoint_with_receipt(
            &active_socket,
            &control_request,
            receipt_wait,
        );
        // A checkpoint sent without a receipt may still be recorded, even if
        // the resend then fails outright.
        let mut maybe_recorded = matches!(
            send_result,
            Err(crate::daemon::CheckpointSendError::Unconfirmed(_))
        );

        // A failed send almost always means the daemon is down (crashed, never
        // started, or restarting), that it took the connection and closed it
        // without handling the request (overloaded, or shutting down), or
        // that it did not confirm it (stuck, or the connection is queued
        // behind others). Rather than dropping the checkpoint — which
        // silently loses the session's steps — spawn the daemon and retry
        // once.
        // `ensure_daemon_running` is a no-op when the daemon is already up, so a
        // transient blip is retried too. Only attempt the spawn once per call.
        if send_result.is_err() && !tried_spawn {
            tried_spawn = true;
            match crate::commands::daemon::ensure_daemon_running(CHECKPOINT_DAEMON_SPAWN_TIMEOUT) {
                Ok(spawned) => {
                    active_socket = spawned.control_socket_path;
                    std::thread::sleep(CHECKPOINT_RESEND_PAUSE);
                    send_result = crate::daemon::send_checkpoint_with_receipt(
                        &active_socket,
                        &control_request,
                        receipt_wait,
                    );
                    maybe_recorded |= matches!(
                        send_result,
                        Err(crate::daemon::CheckpointSendError::Unconfirmed(_))
                    );
                }
                Err(e) => {
                    crate::commands::checkpoint_agent::hook_heartbeat::record(
                        preset_name,
                        crate::commands::checkpoint_agent::hook_heartbeat::HookOutcome::Error,
                        Some(&format!("background service unavailable: {}", e)),
                    );
                    exit_checkpoint_not_captured(
                        hook_mode,
                        &format!("Background worker unavailable, checkpoint dropped: {}", e),
                        &format!(
                            "run `{}`, then `autter doctor`",
                            crate::commands::daemon::RESTART_COMMAND
                        ),
                    );
                }
            }
        }
        if perf {
            eprintln!(
                "[perf] checkpoint: ipc_send={:.1}ms",
                t_send.elapsed().as_secs_f64() * 1000.0,
            );
        }
        if let Err(e) = send_result {
            crate::commands::checkpoint_agent::hook_heartbeat::record(
                preset_name,
                crate::commands::checkpoint_agent::hook_heartbeat::HookOutcome::Error,
                Some(&format!("could not reach the background service: {}", e)),
            );
            let message = if maybe_recorded {
                format!("Checkpoint may not have been recorded: {}", e)
            } else {
                format!(
                    "Failed to send checkpoint to background worker, checkpoint dropped: {}",
                    e
                )
            };
            exit_checkpoint_not_captured(
                hook_mode,
                &message,
                &format!(
                    "run `{}`, then `autter doctor`",
                    crate::commands::daemon::RESTART_COMMAND
                ),
            );
        }
        if is_activity {
            activity_count += 1;
        } else {
            sent_count += 1;
        }
    }

    // Let `autter doctor` tell "installed" from "working" for this agent.
    {
        use crate::commands::checkpoint_agent::hook_heartbeat::{HookOutcome, record};
        if sent_count > 0 {
            record(preset_name, HookOutcome::Checkpoint, None);
        } else if activity_count > 0 {
            record(preset_name, HookOutcome::Activity, None);
        }
    }

    let in_test_harness = std::env::var_os("AUTTER_TEST_DB_PATH").is_some();
    if in_test_harness {
        println!("checkpoint_requests={}", sent_count);
        println!("session_activity_requests={}", activity_count);
    } else if sent_count > 0 && hook_mode {
        warn_if_commits_cannot_be_captured();
    }

    // A direct run gets an honest exit code: the checkpoint is recorded, but if
    // git can't report the commit to autter, the commit won't be attributed.
    // Test harnesses wire trace2 per command, so they opt in explicitly.
    if sent_count > 0
        && !hook_mode
        && (!in_test_harness || env_flag_set("AUTTER_TEST_ENFORCE_CAPTURE_CHECK"))
        && let Some(reason) = commit_capture_problem()
    {
        exit_checkpoint_not_captured(
            false,
            &format!(
                "this checkpoint was recorded, but git isn't set up to report commits to autter ({reason}), so the next commit won't be attributed"
            ),
            "run `autter install --system`, then `autter doctor`",
        );
    }

    if perf {
        eprintln!(
            "[perf] checkpoint: total={:.1}ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// Presets a person or script runs by hand. Every other preset is invoked by an
/// agent hook or an editor extension.
const DIRECT_CHECKPOINT_PRESETS: &[&str] = &["human", "mock_ai", "mock_known_human"];

/// Whether this checkpoint was invoked by an agent hook or editor extension,
/// where a non-zero exit can block or alarm the agent.
///
/// Decided by preset, not only by `--hook-input`: older Gemini/Droid installs,
/// the amp plugin and the IDE extensions (`known_human --editor ...`) call
/// checkpoint without `--hook-input`, and every agent preset exists only for
/// hooks. A manual preset counts as a hook when it carries a hook payload, or
/// when the caller sets `AUTTER_CHECKPOINT_HOOK=1`.
fn checkpoint_runs_in_hook(preset: &str, has_hook_input: bool, hook_env: bool) -> bool {
    !DIRECT_CHECKPOINT_PRESETS.contains(&preset) || has_hook_input || hook_env
}

/// Whether these arguments come from an agent's hook config rather than a
/// person: every hook autter installs passes `--hook-input`, and editor
/// extensions set `AUTTER_CHECKPOINT_HOOK`.
fn invoked_by_agent_hook(args: &[String]) -> bool {
    has_hook_input_arg(args) || env_flag_set("AUTTER_CHECKPOINT_HOOK")
}

/// `--hook-input` on the command line: only an agent's hook config passes it.
/// Used where the subcommand is not a checkpoint, so `AUTTER_CHECKPOINT_HOOK`
/// left in a shell's environment never turns a person's command into a hook.
fn has_hook_input_arg(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--hook-input" || a.starts_with("--hook-input="))
}

static HOOK_NOOP_ANSWERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Print `{}` once. Antigravity reads every hook's stdout as JSON
/// (https://antigravity.google/docs/hooks); `{}` leaves its behavior
/// unchanged: PostToolUse expects `{}`, and a PreToolUse reply without a
/// `decision` is no decision. Claude Code, Codex, Gemini CLI, Droid and
/// Copilot read `{}` as "no fields set", the same as empty output.
fn answer_hook_noop() {
    use std::io::Write as _;
    if HOOK_NOOP_ANSWERED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    println!("{{}}");
    let _ = std::io::stdout().flush();
}

/// Called before any checkpoint work. Antigravity presets (any name starting
/// with `antigravity`, so a renamed one still counts) always get `{}` first.
/// A preset this autter doesn't know, sent by an agent hook, gets it too: we
/// can't tell which agent sent it, and `{}` is a no-op for every agent that
/// parses hook output.
fn answer_checkpoint_hook_early(checkpoint_args: &[String]) {
    let Some(preset) = checkpoint_args.iter().find(|a| !a.starts_with('-')) else {
        return;
    };
    let unknown = crate::commands::checkpoint_agent::presets::resolve_preset(preset).is_err();
    if preset.starts_with("antigravity") || (unknown && invoked_by_agent_hook(checkpoint_args)) {
        answer_hook_noop();
    }
}

/// End a hook invocation whose command this autter can't run as written: the
/// hook config came from another autter version. Exit 0 so the agent carries
/// on, and say on stderr how to fix it.
fn exit_stale_hook(detail: &str) -> ! {
    eprintln!("autter hook is out of date. Run `autter install-hooks` to refresh it ({detail}).");
    std::process::exit(0);
}

fn env_flag_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

/// End a checkpoint that could not be captured. Hooks exit 0 so the agent's
/// edit flow never breaks (the reason still goes to stderr); direct runs exit
/// non-zero with the one command that fixes it.
fn exit_checkpoint_not_captured(hook_mode: bool, message: &str, fix: &str) -> ! {
    if hook_mode {
        eprintln!("{message}");
        std::process::exit(0);
    }
    eprintln!("error: {message}");
    eprintln!("Fix: {fix}");
    std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
}

fn cwd_is_in_git_repository() -> bool {
    std::env::current_dir().is_ok_and(|cwd| {
        crate::git::repository::discover_repository_in_path_no_git_exec(&cwd).is_ok()
    })
}

/// Why a commit made now would not reach the background service, or `None`
/// when it would: git's global trace2 config points at the service, or this
/// environment routes trace2 events itself (`GIT_TRACE2_EVENT`).
fn commit_capture_problem() -> Option<String> {
    if std::env::var_os("GIT_TRACE2_EVENT").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let git_cmd = crate::config::Config::get().git_cmd().to_string();
    let target = crate::diagnostics::GitDiagnosticTarget::new("git", git_cmd);
    let check = crate::diagnostics::check_trace2_global_config(&target);
    (check.status == crate::diagnostics::DiagnosticStatus::Failed).then_some(check.summary)
}

/// A checkpoint only becomes attribution when the commit is observed, and that
/// needs git's global trace2 config pointing at the background service (the
/// git proxy relies on it too). Without it the checkpoint is recorded, status
/// shows the AI lines, and the commit then drops them with no message. Check
/// at most every 10 minutes (it runs `git config`) and warn at most hourly.
fn warn_if_commits_cannot_be_captured() {
    use crate::authorship::stranded_commits::should_warn_now;
    if !should_warn_now("capture_config_checked", 600) {
        return;
    }
    let git_cmd = crate::config::Config::get().git_cmd().to_string();
    let target = crate::diagnostics::GitDiagnosticTarget::new("git", git_cmd);
    let check = crate::diagnostics::check_trace2_global_config(&target);
    if check.status == crate::diagnostics::DiagnosticStatus::Failed
        && should_warn_now("capture_not_configured", 3600)
    {
        eprintln!(
            "[autter] warning: this checkpoint was recorded, but git isn't set up to report commits to autter ({}), so the commit won't be attributed. Run `autter install --system`, then `autter doctor`.",
            check.summary
        );
    }
}

/// Report a checkpoint usage/argument error to both telemetry backends
/// (PostHog Error Tracking + the org database). These indicate a misconfigured
/// caller (a script, CI job, or agent integration invoking `autter checkpoint`
/// incorrectly), so surfacing them helps catch integration bugs.
fn report_checkpoint_usage_error(message: &str) {
    crate::observability::report_cli_error(
        "checkpoint_usage_error",
        message,
        Some("checkpoint"),
        None,
        true,
    );
}

fn strip_utf8_bom(input: String) -> String {
    if let Some(stripped) = input.strip_prefix('\u{feff}') {
        stripped.to_string()
    } else {
        input
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EffectiveIgnorePatternsRequest {
    user_patterns: Vec<String>,
    extra_patterns: Vec<String>,
}

#[derive(Debug, Serialize)]
struct EffectiveIgnorePatternsResponse {
    patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlameAnalysisRequest {
    file_path: String,
    #[serde(default)]
    options: commands::blame::AutterBlameOptions,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorshipRemoteRequest {
    remote_name: String,
}

#[derive(Debug, Serialize)]
struct FetchAuthorshipNotesResponse {
    notes_existence: String,
}

#[derive(Debug, Serialize)]
struct PushAuthorshipNotesResponse {
    ok: bool,
}

fn parse_machine_json_arg(args: &[String], command: &str) -> Result<String, String> {
    if args.len() != 2 || args[0] != "--json" {
        return Err(format!("Usage: autter {} --json '<json-payload>'", command));
    }

    let payload = strip_utf8_bom(args[1].clone());
    if payload.trim().is_empty() {
        return Err("JSON payload cannot be empty".to_string());
    }

    Ok(payload)
}

fn emit_machine_json_error(message: impl AsRef<str>) -> ! {
    let payload = serde_json::json!({ "error": message.as_ref() });
    if let Ok(json) = serde_json::to_string(&payload) {
        eprintln!("{}", json);
    } else {
        eprintln!(r#"{{"error":"failed to serialize error payload"}}"#);
    }
    std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
}

fn print_machine_json(value: &serde_json::Value) {
    match serde_json::to_string(value) {
        Ok(json) => println!("{}", json),
        Err(e) => emit_machine_json_error(format!("Failed to serialize JSON output: {}", e)),
    }
}

fn disable_debug_logs_for_machine_command() {
    // SAFETY: autter command handlers run on the main thread and mutate process env
    // before spawning any worker threads for these internal machine commands.
    unsafe {
        std::env::set_var("AUTTER_DEBUG", "0");
        std::env::remove_var("AUTTER_DEBUG_PERFORMANCE");
    }
}

fn parse_authorship_remote_request(
    args: &[String],
    command: &str,
) -> (Repository, AuthorshipRemoteRequest) {
    let payload =
        parse_machine_json_arg(args, command).unwrap_or_else(|msg| emit_machine_json_error(msg));

    let request: AuthorshipRemoteRequest = serde_json::from_str(&payload)
        .unwrap_or_else(|e| emit_machine_json_error(format!("Invalid JSON payload: {}", e)));

    if request.remote_name.trim().is_empty() {
        emit_machine_json_error("remote_name cannot be empty");
    }

    let repo = find_repository(&Vec::<String>::new())
        .unwrap_or_else(|e| emit_machine_json_error(format!("Failed to find repository: {}", e)));

    (repo, request)
}

fn notes_existence_label(existence: NotesExistence) -> &'static str {
    match existence {
        NotesExistence::Found => "found",
        NotesExistence::NotFound => "not_found",
    }
}

fn handle_effective_ignore_patterns_internal(args: &[String]) {
    let payload = parse_machine_json_arg(args, "effective-ignore-patterns")
        .unwrap_or_else(|msg| emit_machine_json_error(msg));

    let request: EffectiveIgnorePatternsRequest = serde_json::from_str(&payload)
        .unwrap_or_else(|e| emit_machine_json_error(format!("Invalid JSON payload: {}", e)));

    let repo = find_repository(&Vec::<String>::new())
        .unwrap_or_else(|e| emit_machine_json_error(format!("Failed to find repository: {}", e)));

    let response = EffectiveIgnorePatternsResponse {
        patterns: effective_ignore_patterns(&repo, &request.user_patterns, &request.extra_patterns),
    };

    let response_value = serde_json::to_value(response).unwrap_or_else(|e| {
        emit_machine_json_error(format!("Failed to serialize command response: {}", e))
    });
    print_machine_json(&response_value);
}

fn handle_blame_analysis_internal(args: &[String]) {
    let payload = parse_machine_json_arg(args, "blame-analysis")
        .unwrap_or_else(|msg| emit_machine_json_error(msg));

    let request: BlameAnalysisRequest = serde_json::from_str(&payload)
        .unwrap_or_else(|e| emit_machine_json_error(format!("Invalid JSON payload: {}", e)));

    if request.file_path.trim().is_empty() {
        emit_machine_json_error("file_path cannot be empty");
    }

    let repo = find_repository(&Vec::<String>::new())
        .unwrap_or_else(|e| emit_machine_json_error(format!("Failed to find repository: {}", e)));

    let analysis = repo
        .blame_analysis(&request.file_path, &request.options)
        .unwrap_or_else(|e| emit_machine_json_error(format!("blame_analysis failed: {}", e)));

    let response_value = serde_json::to_value(analysis).unwrap_or_else(|e| {
        emit_machine_json_error(format!("Failed to serialize command response: {}", e))
    });
    print_machine_json(&response_value);
}

fn handle_fetch_authorship_notes_internal(args: &[String]) {
    disable_debug_logs_for_machine_command();
    let (repo, request) = parse_authorship_remote_request(args, "fetch-authorship-notes");

    let notes_existence = fetch_authorship_notes(&repo, &request.remote_name).unwrap_or_else(|e| {
        emit_machine_json_error(format!("fetch_authorship_notes failed: {}", e))
    });

    let response = FetchAuthorshipNotesResponse {
        notes_existence: notes_existence_label(notes_existence).to_string(),
    };
    let response_value = serde_json::to_value(response).unwrap_or_else(|e| {
        emit_machine_json_error(format!("Failed to serialize command response: {}", e))
    });
    print_machine_json(&response_value);
}

fn handle_push_authorship_notes_internal(args: &[String]) {
    disable_debug_logs_for_machine_command();
    let (repo, request) = parse_authorship_remote_request(args, "push-authorship-notes");

    push_authorship_notes(&repo, &request.remote_name).unwrap_or_else(|e| {
        emit_machine_json_error(format!("push_authorship_notes failed: {}", e))
    });

    let response = PushAuthorshipNotesResponse { ok: true };
    let response_value = serde_json::to_value(response).unwrap_or_else(|e| {
        emit_machine_json_error(format!("Failed to serialize command response: {}", e))
    });
    print_machine_json(&response_value);
}

fn handle_ai_blame(args: &[String]) {
    // LeadingOnly + no -C recognition: blame forwards git-style flags (e.g. `-C`
    // means copy detection here). Any leading `-C <path>` was already consumed
    // at the top level in handle_autter.
    use crate::commands::output::{self, ErrorKind};
    let pp = match commands::arg_parser::pre_parse(
        args,
        commands::arg_parser::ScanMode::LeadingOnly,
        false,
    ) {
        Ok(p) => p,
        Err(e) => output::fail(ErrorKind::Usage, format!("error: {}", e)),
    };
    if pp.flags.help {
        commands::arg_parser::print_command_help("blame");
        return;
    }
    commands::arg_parser::merge_global_flags(&pp.flags);
    // `blame` and `diff` parse their own trailing `--json` (after the file or
    // revision, where the shared pre-parser stops). Record it globally as
    // well, so their errors are JSON too.
    commands::arg_parser::merge_json_flag_from(&pp.rest);
    // `-i`/`--interactive` is autter's own (git blame has no `-i`), so it is
    // taken out before the git-compatible flags are parsed.
    let interactive = pp
        .rest
        .iter()
        .any(|arg| arg == "-i" || arg == "--interactive");
    let args: Vec<String> = pp
        .rest
        .iter()
        .filter(|arg| *arg != "-i" && *arg != "--interactive")
        .cloned()
        .collect();
    let args = &args;

    if args.is_empty() {
        let current_dir = env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .to_string_lossy()
            .to_string();
        if let Ok(repo) = find_repository_in_path(&current_dir) {
            commands::usage_hints::eprint_missing_blame_args(&repo);
        } else {
            eprintln!("Usage: autter blame <file>");
        }
        output::fail(ErrorKind::Usage, "Error: blame requires a file argument");
    }

    // Find the git repository from current directory
    let current_dir = env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .to_string();
    let repo = match find_repository_in_path(&current_dir) {
        Ok(repo) => repo,
        Err(e) => commands::exit_repository_error("blame", &e),
    };

    // Parse blame arguments
    let (file_path, mut options) = match commands::blame::parse_blame_args(args) {
        Ok(result) => result,
        Err(e) => output::fail(
            ErrorKind::Usage,
            format!("Failed to parse blame arguments: {}", e),
        ),
    };
    // Honor a global `--json` supplied before the file positional (stripped by
    // the shared pre-parser) as well as blame's own trailing `--json`.
    options.json = options.json || commands::arg_parser::json();

    // Auto-detect ignore-revs-file if not explicitly provided, not disabled via --no-ignore-revs-file,
    // and git version supports --ignore-revs-file (git >= 2.23)
    if options.ignore_revs_file.is_none()
        && !options.no_ignore_revs_file
        && repo.git_supports_ignore_revs_file()
    {
        // First, check git config for blame.ignoreRevsFile
        if let Ok(Some(config_path)) = repo.config_get_str("blame.ignoreRevsFile")
            && !config_path.is_empty()
        {
            // Config path could be relative to repo root or absolute
            if let Ok(workdir) = repo.workdir() {
                let full_path = if std::path::Path::new(&config_path).is_absolute() {
                    std::path::PathBuf::from(&config_path)
                } else {
                    workdir.join(&config_path)
                };
                if full_path.exists() {
                    options.ignore_revs_file = Some(full_path.to_string_lossy().to_string());
                }
            }
        }

        // If still not set, check for .git-blame-ignore-revs in the repository root
        if options.ignore_revs_file.is_none()
            && let Ok(workdir) = repo.workdir()
        {
            let ignore_revs_path = workdir.join(".git-blame-ignore-revs");
            if ignore_revs_path.exists() {
                options.ignore_revs_file = Some(ignore_revs_path.to_string_lossy().to_string());
            }
        }
    }

    // Check if this is an interactive terminal
    let is_interactive = std::io::stdout().is_terminal();

    if is_interactive && options.incremental {
        // For incremental mode in interactive terminal, we need special handling
        // This would typically involve a pager like less
        output::fail(
            ErrorKind::Usage,
            "Error: incremental mode is not supported in interactive terminal",
        );
    }

    let file_path = if !std::path::Path::new(&file_path).is_absolute() {
        let current_dir_path = std::path::PathBuf::from(&current_dir);
        current_dir_path
            .join(&file_path)
            .to_string_lossy()
            .to_string()
    } else {
        file_path
    };

    if interactive {
        if commands::interactive::available() && !options.json {
            if let Err(e) = commands::interactive::blame::run(&repo, &file_path, &options) {
                output::fail(ErrorKind::Runtime, format!("Blame failed: {}", e));
            }
            return;
        }
        eprintln!("autter blame -i needs a terminal; showing plain blame.");
    }

    if let Err(e) = repo.blame(&file_path, &options) {
        output::fail(output::error_kind(&e), format!("Blame failed: {}", e));
    }
}

fn handle_ai_diff(args: &[String]) {
    use crate::commands::output::{self, ErrorKind};
    let pp = match commands::arg_parser::pre_parse(
        args,
        commands::arg_parser::ScanMode::LeadingOnly,
        false,
    ) {
        Ok(p) => p,
        Err(e) => output::fail(ErrorKind::Usage, format!("error: {}", e)),
    };
    if pp.flags.help {
        commands::arg_parser::print_command_help("diff");
        return;
    }
    commands::arg_parser::merge_global_flags(&pp.flags);
    // `blame` and `diff` parse their own trailing `--json` (after the file or
    // revision, where the shared pre-parser stops). Record it globally as
    // well, so their errors are JSON too.
    commands::arg_parser::merge_json_flag_from(&pp.rest);
    let args = &pp.rest;

    let current_dir = env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .to_string();
    let repo = match find_repository_in_path(&current_dir) {
        Ok(repo) => repo,
        Err(e) => commands::exit_repository_error("diff", &e),
    };

    if let Err(e) = commands::diff::handle_diff(&repo, args) {
        output::fail(output::error_kind(&e), format!("Diff failed: {}", e));
    }
}

fn handle_stats(args: &[String]) {
    use crate::commands::output::{self, ErrorKind};

    let pp =
        match commands::arg_parser::pre_parse(args, commands::arg_parser::ScanMode::Full, false) {
            Ok(p) => p,
            Err(e) => output::fail(ErrorKind::Usage, format!("error: {}", e)),
        };
    if pp.flags.help {
        commands::arg_parser::print_command_help("stats");
        return;
    }
    commands::arg_parser::merge_global_flags(&pp.flags);
    let args = &pp.rest;

    // Find the git repository
    let repo = match find_repository(&Vec::<String>::new()) {
        Ok(repo) => repo,
        Err(e) => commands::exit_repository_error("stats", &e),
    };
    // Parse stats-specific arguments
    let json_output = commands::arg_parser::json();
    let strict = output::strict();
    let mut commit_sha = None;
    let mut commit_range: Option<CommitRange> = None;
    let mut ignore_patterns: Vec<String> = Vec::new();
    let mut wait_secs: Option<u64> = None;
    let mut interactive = false;
    let mut coverage = false;
    let mut raw_spec: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-i" | "--interactive" => {
                interactive = true;
                i += 1;
            }
            "--coverage" => {
                coverage = true;
                i += 1;
            }
            "--ignore" => {
                // Collect all arguments after --ignore until we hit another flag or commit SHA
                // This supports shell glob expansion: `--ignore *.lock` expands to `--ignore Cargo.lock package.lock`
                i += 1;
                let mut found_pattern = false;
                while i < args.len() {
                    let arg = &args[i];
                    // Stop if we hit another flag
                    if arg.starts_with("--") {
                        break;
                    }
                    // Stop if this looks like a commit SHA or range (contains ..)
                    if arg.contains("..")
                        || (commit_sha.is_none() && !found_pattern && arg.len() >= 7)
                    {
                        // Could be a commit SHA, stop collecting patterns
                        break;
                    }
                    ignore_patterns.push(arg.clone());
                    found_pattern = true;
                    i += 1;
                }
                if !found_pattern {
                    output::fail(
                        ErrorKind::Usage,
                        "--ignore requires at least one pattern argument",
                    );
                }
            }
            "--wait" => {
                wait_secs = Some(30);
                i += 1;
            }
            flag if flag.starts_with("--wait=") => {
                let raw = &flag["--wait=".len()..];
                match raw.parse::<u64>() {
                    Ok(secs) => wait_secs = Some(secs.max(1)),
                    Err(_) => output::fail(
                        ErrorKind::Usage,
                        "--wait expects a positive number of seconds",
                    ),
                }
                i += 1;
            }
            _ => {
                // First non-flag argument is treated as commit SHA or range
                if commit_sha.is_none() && commit_range.is_none() {
                    let arg = &args[i];
                    raw_spec = Some(arg.clone());
                    // Check if this is a commit range (contains "..")
                    if arg.contains("..") {
                        let parts: Vec<&str> = arg.split("..").collect();
                        if parts.len() == 2 {
                            match CommitRange::new_infer_refname(
                                &repo,
                                normalize_head_rev(parts[0]),
                                normalize_head_rev(parts[1]),
                                // @todo this is probably fine, but we might want to give users an option to override from this command.
                                None,
                            ) {
                                Ok(range) => {
                                    commit_range = Some(range);
                                }
                                Err(e) => output::fail(
                                    ErrorKind::RevisionNotFound,
                                    format!("Failed to create commit range: {}", e),
                                ),
                            }
                        } else {
                            output::fail(
                                ErrorKind::Usage,
                                "Invalid commit range format. Expected: <commit>..<commit>",
                            );
                        }
                    } else if arg.starts_with('-') {
                        // A revision never starts with `-`: this is a mistyped flag.
                        commands::arg_parser::exit_unknown_option("stats", arg);
                    } else {
                        commit_sha = Some(normalize_head_rev(arg));
                    }
                    i += 1;
                } else {
                    output::fail(
                        ErrorKind::Usage,
                        format!("Unknown stats argument: {}", args[i]),
                    );
                }
            }
        }
    }

    // `--coverage`: which commits have a record, why the rest don't, and
    // whether the records reached the remote.
    if coverage {
        if let Err(e) = commands::coverage::run_coverage(&repo, raw_spec.as_deref(), json_output) {
            output::fail(ErrorKind::Runtime, format!("Coverage failed: {}", e));
        }
        return;
    }

    // `-i`: browse recent commits full-screen, starting from the given commit.
    if interactive {
        if commands::interactive::available() && !json_output {
            if let Err(e) = commands::interactive::stats::run(&repo, commit_sha.as_deref()) {
                output::fail(ErrorKind::Runtime, e.to_string());
            }
            return;
        }
        eprintln!("autter stats -i needs a terminal; showing the plain summary.");
    }

    let effective_patterns = effective_ignore_patterns(&repo, &ignore_patterns, &[]);

    // Handle commit range if detected
    if let Some(range) = commit_range {
        let (start, end) = (range.start_oid.clone(), range.end_oid.clone());
        match range_authorship::range_authorship(range, false, &effective_patterns, None) {
            Ok(stats) => {
                let report = range_authorship::RangeStatsReport::new(start, end, stats);
                let status = report.summary.capture.status;
                if json_output {
                    if strict && status == CaptureStatus::NothingCaptured {
                        output::fail_with(
                            ErrorKind::NothingCaptured,
                            nothing_captured_message("this range"),
                            Some(&report),
                        );
                    }
                    output::print_json(&report);
                } else {
                    range_authorship::print_range_authorship_stats(&report.stats);
                }
                exit_if_nothing_captured(strict, status, "this range");
            }
            Err(e) => output::fail(
                ErrorKind::Runtime,
                format!("Range authorship failed: {}", e),
            ),
        }
        return;
    }

    if let Some(secs) = wait_secs {
        let sha = match commit_sha.as_deref() {
            Some(s) => match repo.revparse_single(s) {
                Ok(obj) => obj.id(),
                Err(e) => output::fail(
                    ErrorKind::RevisionNotFound,
                    format!("Failed to resolve commit: {e}"),
                ),
            },
            None => match repo.head().and_then(|h| h.target()) {
                Ok(s) => s,
                Err(e) => output::fail(ErrorKind::Runtime, format!("Failed to resolve HEAD: {e}")),
            },
        };
        let timeout = std::time::Duration::from_secs(secs);
        let poll = std::time::Duration::from_millis(100);
        let start = std::time::Instant::now();
        let mut found = false;
        while start.elapsed() < timeout {
            if crate::git::notes_api::read_note(&repo, &sha).is_some() {
                found = true;
                break;
            }
            std::thread::sleep(poll);
        }
        if !found {
            #[derive(Serialize)]
            struct Pending<'a> {
                pending: bool,
                commit: &'a str,
                waited_secs: u64,
            }
            output::fail_with(
                ErrorKind::NotePending,
                format!(
                    "pending: authorship note for {} not ready after {}s (daemon still processing?)",
                    &sha[..std::cmp::min(8, sha.len())],
                    secs
                ),
                Some(&Pending {
                    pending: true,
                    commit: &sha,
                    waited_secs: secs,
                }),
            );
        }
    }

    let report = match commit_stats_report(&repo, commit_sha.as_deref(), &effective_patterns) {
        Ok(report) => report,
        Err(crate::error::AutterError::Generic(msg)) if msg.starts_with("No commit found:") => {
            output::fail(ErrorKind::RevisionNotFound, msg)
        }
        Err(_) if commit_sha.is_none() && commands::repository_has_no_commits(&repo) => {
            output::fail(ErrorKind::Runtime, commands::no_commits_message("stats"))
        }
        Err(e) => output::fail(ErrorKind::Runtime, format!("Stats failed: {}", e)),
    };
    let status = report.summary.capture.status;
    if json_output && strict && status == CaptureStatus::NothingCaptured {
        output::fail_with(
            ErrorKind::NothingCaptured,
            nothing_captured_message("this commit"),
            Some(&report),
        );
    }
    print_commit_stats_report(&report, json_output);
    exit_if_nothing_captured(strict, status, "this commit");
}

/// The `--strict` failure message for `stats`.
fn nothing_captured_message(what: &str) -> String {
    format!(
        "nothing captured: autter has no record of the lines added in {what} (--strict). Run `autter doctor` to check capture."
    )
}

/// `--strict` without `--json`: the human output is already printed; exit 3
/// when nothing was captured.
fn exit_if_nothing_captured(strict: bool, status: CaptureStatus, what: &str) {
    if strict && status == CaptureStatus::NothingCaptured {
        crate::commands::output::fail(
            crate::commands::output::ErrorKind::NothingCaptured,
            nothing_captured_message(what),
        );
    }
}

// Normalise a revision token that the user may have typed with a lowercase
/// "head" prefix.  On case-insensitive file systems (macOS) git accepts both
/// "head" and "HEAD", but in a linked worktree "head" can resolve to the
/// *main* repository's HEAD file rather than the worktree's own HEAD, so the
/// wrong commit is used.  On case-sensitive file systems (Linux) "head"
/// simply fails with "Not a valid revision".  Normalising to uppercase "HEAD"
/// before passing to git fixes both issues.
///
/// Only the four-character prefix is replaced; suffixes like `~2`, `^1` or
/// `@{0}` are preserved verbatim.
fn normalize_head_rev(rev: &str) -> String {
    if rev.len() >= 4 && rev[..4].eq_ignore_ascii_case("head") {
        let suffix = &rev[4..];
        if suffix.is_empty()
            || suffix.starts_with('~')
            || suffix.starts_with('^')
            || suffix.starts_with('@')
        {
            return format!("HEAD{}", suffix);
        }
    }
    rev.to_string()
}

fn handle_git_hooks(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("remove") | Some("uninstall") => {
            let repo = match find_repository(&Vec::<String>::new()) {
                Ok(repo) => repo,
                Err(e) => {
                    eprintln!("Failed to find repository: {}", e);
                    std::process::exit(1);
                }
            };

            match commands::git_hook_handlers::remove_repo_hooks(&repo, false) {
                Ok(report) => {
                    let status = if report.changed { "removed" } else { "ok" };
                    println!(
                        "repo hooks {}: {}",
                        status,
                        report.managed_hooks_path.to_string_lossy()
                    );
                    std::process::exit(crate::commands::EXIT_SUCCESS);
                }
                Err(e) => {
                    eprintln!("Failed to remove repo hooks: {}", e);
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("The git core hooks feature has been sunset.");
            eprintln!("Usage: autter git-hooks remove");
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    }
}

/// Synthesize JSON hook_input from CLI args for mock/test presets that can be
/// invoked without --hook-input.
fn synthesize_hook_input_from_cli_args(preset_name: &str, remaining_args: &[String]) -> String {
    match preset_name {
        "human" | "mock_ai" | "mock_known_human" => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let mut paths: Vec<String> = remaining_args
                .iter()
                .filter(|a| !a.starts_with("--"))
                .map(|s| {
                    let p = std::path::Path::new(s.as_str());
                    if p.is_absolute() {
                        s.clone()
                    } else {
                        cwd.join(p).to_string_lossy().to_string()
                    }
                })
                .collect();
            if paths.is_empty() {
                paths = discover_dirty_files_from_status(&cwd);
            }
            serde_json::json!({
                "file_paths": paths,
                "cwd": cwd.to_string_lossy(),
            })
            .to_string()
        }
        "known_human" => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let mut editor = "unknown".to_string();
            let mut editor_version = "unknown".to_string();
            let mut extension_version = "unknown".to_string();
            let mut files: Vec<String> = Vec::new();
            let mut i = 0usize;
            while i < remaining_args.len() {
                match remaining_args[i].as_str() {
                    "--editor" if i + 1 < remaining_args.len() => {
                        editor = remaining_args[i + 1].clone();
                        i += 2;
                    }
                    "--editor-version" if i + 1 < remaining_args.len() => {
                        editor_version = remaining_args[i + 1].clone();
                        i += 2;
                    }
                    "--extension-version" if i + 1 < remaining_args.len() => {
                        extension_version = remaining_args[i + 1].clone();
                        i += 2;
                    }
                    "--" => {
                        files.extend(remaining_args[i + 1..].iter().map(|s| {
                            let p = std::path::Path::new(s.as_str());
                            if p.is_absolute() {
                                s.clone()
                            } else {
                                cwd.join(p).to_string_lossy().to_string()
                            }
                        }));
                        break;
                    }
                    arg if !arg.starts_with("--") => {
                        let p = std::path::Path::new(arg);
                        if p.is_absolute() {
                            files.push(arg.to_string());
                        } else {
                            files.push(cwd.join(p).to_string_lossy().to_string());
                        }
                        i += 1;
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            serde_json::json!({
                "editor": editor,
                "editor_version": editor_version,
                "extension_version": extension_version,
                "cwd": cwd.to_string_lossy(),
                "edited_filepaths": files,
            })
            .to_string()
        }
        _ => String::new(),
    }
}

fn discover_dirty_files_from_status(cwd: &std::path::Path) -> Vec<String> {
    let repo_root = crate::git::repository::discover_repository_in_path_no_git_exec(cwd)
        .ok()
        .and_then(|r| r.workdir().ok())
        .unwrap_or_else(|| cwd.to_path_buf());

    let output = std::process::Command::new(crate::config::Config::get().git_cmd())
        .args(["status", "--porcelain", "-uall"])
        .current_dir(cwd)
        .output()
        .ok();
    let Some(output) = output else {
        return vec![];
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let raw_file = line[3..].trim();
            if raw_file.is_empty() {
                return None;
            }
            let unescaped = crate::utils::unescape_git_path(raw_file);
            let mut file = unescaped.as_str();
            // Renames show as "old_name -> new_name"; take only the new name
            if let Some(arrow_pos) = file.find(" -> ") {
                file = &file[arrow_pos + 4..];
            }
            let p = std::path::Path::new(file);
            if p.is_absolute() {
                Some(file.to_string())
            } else {
                Some(repo_root.join(p).to_string_lossy().to_string())
            }
        })
        .collect()
}

/// Exit mirroring the child's termination status, re-raising the original
/// signal on Unix so the calling shell sees the correct termination reason
/// (e.g. SIGPIPE from `autter log | head`).
fn exit_with_log_status(status: std::process::ExitStatus) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::raise(sig);
            }
            unreachable!();
        }
    }
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod checkpoint_exit_policy_tests {
    use super::checkpoint_runs_in_hook;

    #[test]
    fn agent_presets_and_editor_extensions_are_hooks() {
        for preset in ["claude", "codex", "cursor", "gemini", "amp", "known_human"] {
            assert!(checkpoint_runs_in_hook(preset, false, false), "{preset}");
            assert!(checkpoint_runs_in_hook(preset, true, false), "{preset}");
        }
    }

    #[test]
    fn manual_presets_are_direct_unless_they_carry_a_hook_payload() {
        for preset in ["human", "mock_ai", "mock_known_human"] {
            assert!(!checkpoint_runs_in_hook(preset, false, false), "{preset}");
            assert!(checkpoint_runs_in_hook(preset, true, false), "{preset}");
            assert!(checkpoint_runs_in_hook(preset, false, true), "{preset}");
        }
    }
}

#[cfg(test)]
mod command_list_tests {
    use super::is_autter_command;

    #[test]
    fn every_documented_command_is_a_known_command() {
        for command in crate::commands::arg_parser::documented_commands() {
            assert!(
                is_autter_command(command),
                "`autter {command}` is documented but missing from AUTTER_COMMANDS"
            );
        }
        for command in ["version", "help", "memory-hook", "notes", "checkpoint"] {
            assert!(is_autter_command(command), "{command}");
        }
    }

    #[test]
    fn a_typo_is_not_a_command() {
        for command in ["stat", "bogus", "", "--json", "checkpoints"] {
            assert!(!is_autter_command(command), "{command}");
        }
    }
}
