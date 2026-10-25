//! First-run onboarding flow (`autter onboard`).
//!
//! After installation, autter walks the user through a one-time guided setup
//! on a single step rail: a choice between the two modes, browser sign-in
//! when connecting, a telemetry consent choice, which integrations to
//! install, and a closing summary with the first commands to try. Each
//! question shows its place in the flow ("2/4").
//!
//! - **Local mode** — everything stays on the machine (git notes + local
//!   storage). Nothing is uploaded. The user is told they will not get the
//!   platform's detailed prompt-usage and team/user-usage dashboards.
//!
//! - **Connected mode** — the user links their Autter account (OAuth device
//!   flow). Attribution is still written to local git notes exactly as before,
//!   and prompt/usage data additionally syncs to the org's Autter platform
//!   (persisted in the org's PostgreSQL via the platform API). If sign-in is
//!   skipped or fails, the user can retry, paste an access token, or continue
//!   in local mode.
//!
//! After the mode choice (and regardless of whether the user signed in), the
//! flow asks for consent to anonymous usage analytics and error tracking. The
//! answer is stored in `telemetry_oss` ("on"/"off"); when enabled, events flow
//! to PostHog and are mirrored to a local audit log at
//! `~/.autter/internal/telemetry.log` so the user can see exactly what is sent.
//!
//! The chosen mode is recorded in `~/.autter/config.json` via
//! `onboarding_completed`, so re-running an installer does not nag the user.
//! Non-interactive shells never see prompts: `--connect`/`--local` and
//! `--telemetry`/`--no-telemetry` drive scripted installs, and with no flags
//! the flow defers itself until the user has a real terminal.
//!
//! Choosing a mode is not consent to change system configuration. IDE/agent
//! hooks, global git trace2 settings and the background service are applied
//! only for the integrations the user checks interactively, or all of them
//! with an explicit `--yes`.
//!
//! Note: the full authorship-note → Postgres dual-write (keeping local git
//! notes while also uploading the note itself) is delivered alongside the
//! in-repo backend. Today, connected mode keeps local git notes and uploads
//! prompt/usage data via the existing CAS upload path.

use std::collections::BTreeMap;

use serde_json::json;

use crate::auth::{AuthState, collect_auth_status};
use crate::commands::login::{Decline, LoginOutcome, LoginUi, run_device_login_with};
use crate::config::{self, NotesBackendConfig, NotesBackendKind};
use crate::ui::{self, SelectItem, TermCaps, rail};

/// Entry point for the `autter onboard` command.
pub fn handle_onboard(args: &[String]) {
    // `--help` (in any position) prints help and returns before the flow
    // starts, so requesting help never prompts, writes config, restarts the
    // daemon, or records telemetry. Matches the per-command convention used
    // by e.g. `login`/`whoami` (top-level `help onboard` is handled earlier
    // in `handle_autter`).
    if is_help_request(args) {
        crate::commands::arg_parser::print_command_help("onboard");
        return;
    }

    let OnboardFlags {
        force,
        choose_connect,
        choose_local,
        assume_yes,
    } = OnboardFlags::parse(args);
    // Non-interactive telemetry overrides (for scripted installs).
    let telemetry_flag = if args.iter().any(|a| a == "--telemetry") {
        Some(true)
    } else if args.iter().any(|a| a == "--no-telemetry") {
        Some(false)
    } else {
        None
    };

    let mut file_config = config::load_file_config_public().unwrap_or_default();

    // Already onboarded and no explicit override: just show status and exit.
    if file_config.onboarding_completed == Some(true) && !force && !choose_connect && !choose_local
    {
        print_status_summary();
        print_telemetry_summary(&file_config);
        eprintln!();
        eprintln!("Already set up. Re-run with `autter onboard --force` to change your choice.");
        return;
    }

    let logged_in = matches!(collect_auth_status().state, AuthState::LoggedIn);
    let interactive = ui::is_interactive();
    let ask_mode = !(choose_connect || choose_local || logged_in);

    if ask_mode && !interactive {
        // Non-interactive shell with no explicit choice: don't block automated
        // installs and don't guess — leave onboarding incomplete so the user can
        // run `autter onboard` later from a real terminal.
        eprintln!("Non-interactive shell detected — skipping onboarding for now.");
        eprintln!("Run `autter onboard` from your terminal to choose local or connected mode.");
        return;
    }

    print_welcome();

    let plan = StepPlan {
        ask_mode,
        sign_in: !logged_in,
        ask_telemetry: telemetry_flag.is_none(),
        ask_integrations: !assume_yes,
        maybe_backlog: !assume_yes && interactive && backlog_exists(),
    };
    // Number the questions for the recommended (connected) path until the
    // user picks one.
    let mut steps = Steps::new(interactive, plan.total(true));

    let connect = if choose_connect {
        true
    } else if choose_local {
        false
    } else if logged_in {
        // Already authenticated (e.g. via an install nonce): treat as connected
        // without prompting.
        true
    } else {
        prompt_mode_choice(steps.next().as_deref())
    };
    steps.set_total(plan.total(connect));

    // Every step that does not complete is recorded here, so onboarding never
    // ends with "You're all set!" after something went wrong.
    let mut report = SetupReport::default();

    if connect {
        // Freeze any backlog before signing in: once credentials exist the
        // background service would start draining it, before the user has been
        // asked about it below. Held items are never uploaded until released.
        if let Err(e) = hold_backlog_before_connecting() {
            report.problem(
                format!(
                    "Could not hold the existing upload backlog ({e}); it may upload without being reviewed"
                ),
                crate::upload_hold::PURGE_COMMAND,
            );
        }
        if let Err(e) = setup_connected(&mut file_config, logged_in, !ask_mode, &mut steps) {
            report.problem(
                format!(
                    "Could not connect to the Autter platform ({e}); set up in local mode instead"
                ),
                "autter onboard --connect",
            );
        }
    } else {
        setup_local(&mut file_config, !ask_mode);
    }

    // Telemetry consent is asked regardless of the connected/local choice above.
    let telemetry_enabled = configure_telemetry(&mut file_config, telemetry_flag, &mut steps);

    // System integrations (IDE hooks, global git trace2, daemon) require
    // explicit consent. Picking a mode (`--local`/`--connect`) or re-running
    // (`--force`) is not consent; only interactive choices or `--yes` are.
    if let Err(e) = apply_system_integrations_with_consent(assume_yes, &mut steps) {
        report.problem(
            format!("Could not install IDE/agent hooks and git capture settings ({e})"),
            "autter install --system",
        );
    }

    // After connect: offer to keep or discard any pre-login upload backlog.
    if file_config.prompt_storage.as_deref() != Some("local")
        && let Err(e) = offer_historical_sync_consent(assume_yes, &mut steps)
    {
        report.problem(
            format!("Could not apply your choice for the held upload backlog ({e})"),
            crate::upload_hold::PURGE_COMMAND,
        );
    }

    file_config.onboarding_completed = Some(true);
    if let Err(e) = config::save_file_config(&file_config) {
        let path = config::config_file_path_public()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.autter/config.json".to_string());
        report.problem(
            format!(
                "Could not save your choice to {path} ({e}); the mode was not applied. Make sure that file is writable, then re-run"
            ),
            "autter onboard --force",
        );
    }

    // The background service is what persists authorship notes, and it may
    // have been started under the previous mode's config. Restart it so the
    // new notes backend / prompt storage take effect immediately. A service
    // that is not running is left alone: starting it is a system integration
    // that needs consent (installing integrations above starts it).
    if let Err(e) = restart_daemon_for_mode_change() {
        report.problem(
            format!(
                "Could not restart the background service ({e}), so it may still use the previous mode"
            ),
            crate::commands::daemon::RESTART_COMMAND,
        );
    }

    // Reflect the mode actually configured: a connect attempt can fall back
    // to local if the device login fails.
    let connected = file_config.prompt_storage.as_deref() != Some("local");
    eprint!("{}", render_finished(connected, &report, &ui::caps()));

    // Fire a one-off install/onboard event so we can count installs by version
    // and platform. Only when the user just opted in.
    if telemetry_enabled {
        record_install_event(connected);
    }

    // A degraded setup is a runtime failure for scripts (installers run
    // `autter onboard || true`, so this never aborts an install).
    if !report.is_clean() {
        std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
    }
}

/// Which questions this run will ask, to number them.
#[derive(Debug, Clone, Copy)]
struct StepPlan {
    ask_mode: bool,
    sign_in: bool,
    ask_telemetry: bool,
    ask_integrations: bool,
    maybe_backlog: bool,
}

impl StepPlan {
    fn total(&self, connect: bool) -> usize {
        [
            self.ask_mode,
            connect && self.sign_in,
            self.ask_telemetry,
            self.ask_integrations,
            connect && self.maybe_backlog,
        ]
        .into_iter()
        .filter(|&step| step)
        .count()
    }
}

/// Numbers the questions actually shown ("2/4"). Without a terminal there
/// are no questions, so no numbers either.
struct Steps {
    enabled: bool,
    shown: usize,
    total: usize,
}

impl Steps {
    fn new(enabled: bool, total: usize) -> Self {
        Self {
            enabled,
            shown: 0,
            total,
        }
    }

    fn set_total(&mut self, total: usize) {
        self.total = total;
    }

    fn next(&mut self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        self.shown += 1;
        Some(format!("{}/{}", self.shown, self.total.max(self.shown)))
    }
}

/// Whether there is queued upload data a connect would ask about.
fn backlog_exists() -> bool {
    if std::env::var_os("AUTTER_TEST_DB_PATH").is_some() {
        return false;
    }
    crate::auth::notice::pending_sync_counts().total() + crate::upload_hold::held_counts().total()
        > 0
}

/// A prompt title with its step counter.
fn titled(title: &str, counter: Option<&str>) -> String {
    rail::titled(&ui::caps(), title, counter)
}

/// One onboarding step that did not complete, with the exact command that
/// fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SetupProblem {
    what: String,
    fix: String,
}

/// Degraded steps collected during onboarding.
#[derive(Debug, Default)]
struct SetupReport {
    problems: Vec<SetupProblem>,
}

impl SetupReport {
    fn problem(&mut self, what: impl Into<String>, fix: impl Into<String>) {
        self.problems.push(SetupProblem {
            what: what.into(),
            fix: fix.into(),
        });
    }

    fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Restart an already-running daemon so it re-reads the freshly-saved mode
/// config. Never starts a daemon that is not running: starting the background
/// service is a system integration that needs consent. Skipped in test
/// harnesses, which manage their own daemon lifecycle. A failure is reported
/// to the caller, which lists it in the closing summary.
fn restart_daemon_for_mode_change() -> Result<(), String> {
    if std::env::var_os("AUTTER_TEST_DB_PATH").is_some() {
        return Ok(());
    }

    let daemon_config = crate::daemon::DaemonConfig::from_env_or_default_paths()
        .map_err(|e| format!("could not locate the background service: {e}"))?;
    if !crate::commands::daemon::daemon_is_up(&daemon_config) {
        return Ok(());
    }

    crate::commands::daemon::restart_daemon(&daemon_config)
}

/// The interactive connected-vs-local choice, as an arrow-key selector.
fn prompt_mode_choice(counter: Option<&str>) -> bool {
    let items = [
        SelectItem::new(
            "Connected — link this machine to your Autter org (recommended)",
            "Powers prompt search, team dashboards, and PR authorship views. Sign-in opens in your browser.",
        ),
        SelectItem::new(
            "Local only — everything stays on this machine",
            "No account needed. Switch anytime with `autter onboard --connect`.",
        ),
    ];
    ui::select(
        &titled("How should Autter run on this machine?", counter),
        &items,
        0,
    ) == 0
}

fn telemetry_log_path() -> String {
    config::id_file_path()
        .and_then(|p| p.parent().map(|d| d.join("telemetry.log")))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.autter/internal/telemetry.log".to_string())
}

/// Ask whether to enable anonymous telemetry + error tracking and persist the
/// choice into `telemetry_oss` ("on"/"off"). Returns whether telemetry ended up
/// enabled.
fn configure_telemetry(
    cfg: &mut config::FileConfig,
    flag: Option<bool>,
    steps: &mut Steps,
) -> bool {
    let local_log = telemetry_log_path();

    let enabled = match flag {
        Some(value) => {
            rail::print_step(telemetry_summary(value), None);
            value
        }
        None if ui::is_interactive() => {
            let yes_hint = format!(
                "Sends OS, CPU architecture, core count, the Autter version and a random install ID. \
                 Never code, prompts, file paths, repo names, usernames or IP addresses. \
                 Everything sent is mirrored to {local_log}."
            );
            let items = [
                SelectItem::new("Yes — help improve Autter", &yes_hint),
                SelectItem::new(
                    "No — send nothing",
                    "Change anytime with `autter telemetry on` or `autter telemetry off`.",
                ),
            ];
            let counter = steps.next();
            ui::select(
                &titled(
                    "Share anonymous usage data and error reports?",
                    counter.as_deref(),
                ),
                &items,
                0,
            ) == 0
        }
        None => {
            // Non-interactive and no explicit flag: default to disabled so we
            // never collect data the user didn't actively agree to.
            rail::print_step(telemetry_summary(false), None);
            false
        }
    };

    cfg.telemetry_oss = Some(if enabled { "on" } else { "off" }.to_string());
    if enabled {
        rail::print_note(&format!("Everything sent is mirrored to {local_log}"));
    }
    enabled
}

fn telemetry_summary(enabled: bool) -> &'static str {
    if enabled {
        "Anonymous telemetry on"
    } else {
        "Anonymous telemetry off \u{00B7} nothing is collected or sent"
    }
}

/// Send a single anonymous install/onboard event to PostHog (best-effort).
fn record_install_event(connected: bool) {
    if let Some(client) = crate::telemetry_client::PostHogClient::resolve_unchecked() {
        let distinct_id = config::get_or_create_distinct_id();
        let mut props = BTreeMap::new();
        props.insert(
            "mode".to_string(),
            json!(if connected { "connected" } else { "local" }),
        );
        client.capture(&distinct_id, "autter_installed", props);
    }
}

/// Configure local-only mode and explain the trade-off. `announce` adds the
/// step line when no mode question was shown (flags, or a fallback).
fn setup_local(cfg: &mut config::FileConfig, announce: bool) {
    // Keep everything on the machine: prompts only in local SQLite, attribution
    // in local git notes, nothing uploaded.
    cfg.prompt_storage = Some("local".to_string());
    cfg.notes_backend = Some(NotesBackendConfig {
        kind: NotesBackendKind::GitNotes,
        backend_url: None,
    });

    if announce {
        rail::print_step("Local mode \u{00B7} everything stays on this machine", None);
    }
    rail::print_note(
        "Attribution, blame and stats run on this machine, in git notes (refs/notes/ai).\n\
         Nothing is uploaded, so the platform's prompt-usage and team dashboards aren't included.\n\
         Connect anytime with `autter onboard --connect`.",
    );

    // Data queued while this machine was connected (or by older versions)
    // stays on disk. Local mode never uploads it; reconnecting asks before
    // importing it. Tell the user it exists and how to delete it.
    if std::env::var_os("AUTTER_TEST_DB_PATH").is_none() {
        let pending = crate::auth::notice::pending_sync_counts();
        let held = crate::upload_hold::held_counts();
        if pending.total() + held.total() > 0 {
            rail::print_note(&format!(
                "Found {} records queued for upload earlier. They won't be uploaded in local mode.\n\
                 Delete them with `autter sync purge --force`.",
                pending.total() + held.total()
            ));
        }
    }
}

/// Log the user in (if needed) and configure connected mode. Skipping
/// sign-in sets up local mode; a failed sign-in does too, and returns the
/// error so the closing summary can report it.
fn setup_connected(
    cfg: &mut config::FileConfig,
    already_logged_in: bool,
    announce: bool,
    steps: &mut Steps,
) -> Result<(), String> {
    if !already_logged_in {
        let interactive = ui::is_interactive();
        if !interactive {
            eprintln!();
            eprintln!("Connecting to the Autter platform...");
        }
        let counter = steps.next();
        let outcome = run_device_login_with(LoginUi {
            counter: counter.as_deref(),
            decline: Decline::ContinueLocally,
        });
        match outcome {
            Ok(LoginOutcome::LoggedIn | LoginOutcome::AlreadyLoggedIn) => {}
            Ok(LoginOutcome::Declined { after_error: None }) => {
                setup_local(cfg, true);
                return Ok(());
            }
            Ok(LoginOutcome::Declined {
                after_error: Some(e),
            }) => {
                setup_local(cfg, true);
                return Err(e);
            }
            Err(e) => {
                eprintln!();
                eprintln!("\u{2717} Could not connect: {e}");
                eprintln!();
                crate::commands::login::print_pat_fallback_instructions();
                // `autter login` alone does not switch modes: local mode never
                // uploads, so the retry has to go through onboarding.
                eprintln!(
                    "  Setting up local mode for now \u{2014} run `autter onboard --connect` to retry."
                );
                setup_local(cfg, true);
                return Err(e);
            }
        }
    }

    // Connected mode: upload prompts (CAS) and authorship notes to the hosted
    // data plane. `backend_url: None` resolves to DEFAULT_NOTES_BACKEND_URL
    // (the API base) via Config::notes_backend_url().
    cfg.prompt_storage = Some("default".to_string());
    cfg.notes_backend = Some(NotesBackendConfig {
        kind: NotesBackendKind::Http,
        backend_url: None,
    });

    // A fresh sign-in already showed who is signed in.
    if already_logged_in && announce {
        let status = collect_auth_status();
        let who = status
            .email
            .or(status.name)
            .unwrap_or_else(|| "your Autter account".to_string());
        rail::print_step(&format!("Connected as {who}"), None);
    }
    rail::print_note(
        "Attribution is still written locally to git notes (refs/notes/ai).\n\
         Prompt and usage data also sync to your org's Autter dashboard.",
    );
    Ok(())
}

fn print_welcome() {
    let version = env!("CARGO_PKG_VERSION");
    eprintln!();
    ui::Header::new(
        "Welcome to Autter",
        format!("v{version}"),
        "Setup \u{00B7} about a minute",
        ui::header::display_cwd(),
    )
    .print();
    eprintln!();
    eprintln!("  Autter records which lines of your code were written by AI \u{2014} tied to");
    eprintln!("  the agent, model, and prompts behind them \u{2014} right in Git itself.");
    eprintln!();
}

/// Closing summary. With no problems: "You're all set!" plus the first
/// commands worth trying. With problems: "Set up with warnings", each problem
/// with the exact command that fixes it, and no claim that everything is
/// ready.
fn render_finished(connected: bool, report: &SetupReport, caps: &TermCaps) -> String {
    let bar = caps.paint("90", caps.glyphs().bar);
    let cyan = |text: &str| caps.paint("36", text);
    let mut out = rail::gap(caps);
    if report.is_clean() {
        out.push_str(&rail::end(
            caps,
            &format!(
                "{} Authorship is recorded automatically on every commit.",
                caps.paint("1;32", "You're all set!")
            ),
        ));
    } else {
        let count = report.problems.len();
        out.push_str(&rail::warn(
            caps,
            &format!(
                "Set up with warnings \u{2014} {count} step{} did not complete:",
                if count == 1 { "" } else { "s" }
            ),
        ));
        for problem in &report.problems {
            let mut lines = problem.what.lines();
            if let Some(first) = lines.next() {
                out.push_str(&format!(
                    "{bar}  {} {first}\n",
                    caps.paint("33", "\u{2022}")
                ));
            }
            for line in lines {
                out.push_str(&format!("{bar}    {line}\n"));
            }
            out.push_str(&format!("{bar}    Fix: {}\n", cyan(&problem.fix)));
        }
        out.push_str(&rail::end(
            caps,
            &format!(
                "Then run {} to confirm everything works.",
                cyan("autter doctor")
            ),
        ));
    }

    let command = |name: &str, what: &str| {
        format!(
            "     {}{}  {}\n",
            cyan(name),
            " ".repeat(20usize.saturating_sub(name.len())),
            caps.paint("2", what)
        )
    };
    out.push('\n');
    out.push_str("   Try it on any repo:\n");
    out.push_str(&command(
        "autter stats",
        "AI vs human share of your latest commit",
    ));
    out.push_str(&command(
        "autter blame <file>",
        "who \u{2014} you or an agent \u{2014} wrote each line",
    ));
    out.push_str(&command("autter log", "git log with AI authorship stats"));
    if connected {
        out.push_str(&command("autter dash", "open your personal dashboard"));
    }
    out.push('\n');
    out.push_str(&format!(
        "   {}\n",
        caps.paint(
            "2",
            "All commands: `autter help` \u{2022} Docs: https://autter.dev/docs"
        )
    ));
    out
}

fn print_status_summary() {
    let status = collect_auth_status();
    if !config::Config::fresh().platform_sync_enabled() {
        eprintln!("Autter is running in local mode: nothing is uploaded to the Autter platform.");
        if matches!(status.state, AuthState::LoggedIn) {
            eprintln!("  You are signed in, but local mode keeps all data on this machine.");
        }
        eprintln!();
        eprintln!("  To connect:  autter onboard --connect");
        return;
    }
    if matches!(status.state, AuthState::LoggedIn) {
        let who = status
            .email
            .clone()
            .or(status.name.clone())
            .unwrap_or_else(|| "your Autter account".to_string());
        eprintln!("Autter is connected to the platform as {who}.");
    } else {
        eprintln!("Autter is running in local mode (not connected to the platform).");
        eprintln!();
        eprintln!("  Why connect? Linking this machine to the Autter platform lets you:");
        eprintln!(
            "    \u{2022} See detailed prompt usage \u{2014} the prompts and model behind each AI change"
        );
        eprintln!(
            "    \u{2022} Track AI vs human authorship across your whole team, not just this machine"
        );
        eprintln!("    \u{2022} View team/user usage dashboards and trends over time");
        eprintln!();
        eprintln!(
            "  Your attribution stays in local git notes (refs/notes/ai) either way; connecting"
        );
        eprintln!("  additionally syncs prompt and usage data to your org's Autter dashboard.");
        eprintln!();
        eprintln!("  To connect:  autter onboard --connect");
    }
}

/// Show the current telemetry setting and where to inspect what's sent.
fn print_telemetry_summary(cfg: &config::FileConfig) {
    let enabled = cfg.telemetry_oss.as_deref() != Some("off");
    eprintln!();
    if enabled {
        eprintln!("Anonymous telemetry is ON. Everything sent is logged at:");
        eprintln!("  {}", telemetry_log_path());
    } else {
        eprintln!("Anonymous telemetry is OFF.");
    }
}

/// Install IDE hooks + global git trace2 + start the daemon after consent.
/// Returns whether any integrations were installed, or the install error.
///
/// Only `assume_yes` (`--yes`) applies them all without asking. Interactive
/// runs list the detected tools with everything checked, so Enter installs
/// everything and unchecking all is "not now"; non-interactive runs without
/// `--yes` leave them off.
fn apply_system_integrations_with_consent(
    assume_yes: bool,
    steps: &mut Steps,
) -> Result<bool, String> {
    if std::env::var_os("AUTTER_TEST_DB_PATH").is_some() {
        return Ok(false);
    }

    match integration_decision(assume_yes, ui::is_interactive()) {
        IntegrationDecision::Apply => {
            rail::print_step("Installing integrations", None);
            let _rail = ui::progress::on_rail();
            crate::commands::install_hooks::run(&["--system".to_string()])
                .map(|_| true)
                .map_err(|e| e.to_string())
        }
        IntegrationDecision::Skip => {
            // Non-interactive without `--yes`: leave integrations off.
            eprintln!(
                "Skipping system integrations (non-interactive, no --yes). Run `autter install --system` to install them."
            );
            Ok(false)
        }
        IntegrationDecision::Ask => {
            let spinner =
                ui::progress::Spinner::new("Looking for coding tools on this machine\u{2026}");
            let tools = crate::commands::install_hooks::detect_tools();
            spinner.stop();

            let mut items: Vec<SelectItem> = tools
                .iter()
                .map(|tool| {
                    SelectItem::new(
                        &tool.name,
                        "Hooks record the edits this tool's agent makes.",
                    )
                })
                .collect();
            items.push(SelectItem::new(
                "Git capture and background service",
                "Global git trace2 settings and a background service, so commits from any terminal get authorship notes.",
            ));
            let system_item = items.len() - 1;
            let all: Vec<usize> = (0..items.len()).collect();

            let counter = steps.next();
            let question = format!(
                "{}\n{}",
                titled("Install integrations?", counter.as_deref()),
                ui::caps().paint("2", "space toggles \u{00B7} enter confirms")
            );
            let chosen = ui::multiselect(&question, &items, &all);
            if chosen.is_empty() {
                rail::print_note("Skipped. Run `autter install --system` when you're ready.");
                return Ok(false);
            }

            let tool_ids: Vec<String> = chosen
                .iter()
                .filter(|&&i| i < tools.len())
                .map(|&i| tools[i].id.clone())
                .collect();
            crate::commands::install_hooks::run_for_tools(&tool_ids, chosen.contains(&system_item))
                .map(|_| true)
                .map_err(|e| e.to_string())
        }
    }
}

/// Parsed `autter onboard` flags.
#[derive(Debug, Default, PartialEq, Eq)]
struct OnboardFlags {
    /// `--force`/`-f`: re-run onboarding even if it was completed.
    force: bool,
    choose_connect: bool,
    choose_local: bool,
    /// `--yes`/`-y`: consent to system integrations without a prompt. The
    /// only flag that counts as consent.
    assume_yes: bool,
}

impl OnboardFlags {
    fn parse(args: &[String]) -> Self {
        Self {
            force: args.iter().any(|a| a == "--force" || a == "-f"),
            choose_connect: args.iter().any(|a| a == "--connect"),
            choose_local: args.iter().any(|a| a == "--local"),
            assume_yes: args.iter().any(|a| a == "--yes" || a == "-y"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum IntegrationDecision {
    Apply,
    Ask,
    Skip,
}

/// Whether to install system integrations: only explicit consent applies
/// them; otherwise ask when there is a terminal, and skip when there is not.
fn integration_decision(assume_yes: bool, interactive: bool) -> IntegrationDecision {
    if assume_yes {
        IntegrationDecision::Apply
    } else if interactive {
        IntegrationDecision::Ask
    } else {
        IntegrationDecision::Skip
    }
}

/// Hold every queued upload item before connecting (see `crate::upload_hold`).
/// Applies the one-time legacy migration first so it can never later mistake
/// data queued after connecting for pre-upgrade backlog.
fn hold_backlog_before_connecting() -> Result<(), String> {
    crate::upload_hold::ensure_legacy_migration()?;
    crate::upload_hold::hold_all().map(|_| ())
}

/// Ask what to do with the backlog that predates this connect, which
/// [`hold_backlog_before_connecting`] has already held. Nothing in it uploads
/// without explicit consent: an interactive "Import", or `--yes` in a
/// non-interactive run. Otherwise it stays held, and the commands to import
/// or delete it are printed.
fn offer_historical_sync_consent(assume_yes: bool, steps: &mut Steps) -> Result<(), String> {
    let held = crate::upload_hold::held_counts();
    if held.total() == 0 {
        return Ok(());
    }

    rail::print_note(&format!(
        "Found local data queued for upload: {}.\n\
         It was queued before you connected this machine (or by an older autter) and may include\n\
         sessions recorded in local mode. It is held and has not been sent.",
        held.summary()
    ));

    let decision = if ui::is_interactive() {
        let items = [
            SelectItem::new(
                "Import — upload this backlog to your Autter org",
                "Recommended if you want historical attribution on the dashboard.",
            ),
            SelectItem::new(
                "Discard — delete it from this machine",
                "Runs `autter sync purge`. Nothing pre-dating this connect is uploaded.",
            ),
        ];
        let counter = steps.next();
        if ui::select(
            &titled("What should we do with the backlog?", counter.as_deref()),
            &items,
            0,
        ) == 0
        {
            BacklogDecision::Import
        } else {
            BacklogDecision::Discard
        }
    } else if assume_yes {
        BacklogDecision::Import
    } else {
        BacklogDecision::KeepHeld
    };

    match decision {
        BacklogDecision::Import => {
            let released = crate::upload_hold::release_all()?;
            rail::print_note(&format!(
                "Backlog released ({} records); it uploads in the background after setup.",
                released.total()
            ));
        }
        BacklogDecision::Discard => {
            let summary = crate::commands::sync::purge_sync_queues().map_err(|e| e.to_string())?;
            rail::print_step(&format!("Cleared local upload queue ({summary})"), None);
        }
        BacklogDecision::KeepHeld => {
            rail::print_note(&format!(
                "Kept on this machine and NOT uploaded (non-interactive run without --yes).\n\
                 Upload it:  {}\n\
                 Delete it:  {}",
                crate::upload_hold::IMPORT_COMMAND,
                crate::upload_hold::PURGE_COMMAND
            ));
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum BacklogDecision {
    Import,
    Discard,
    KeepHeld,
}

/// True when the args are only asking for help (`--help`/`-h`/`help` in any
/// position). Checked at the top of [`handle_onboard`] so help never starts
/// the interactive flow or touches local state.
fn is_help_request(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--help" || a == "-h" || a == "help")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{Charset, ColorLevel};

    fn args(flags: &[&str]) -> Vec<String> {
        flags.iter().map(|s| s.to_string()).collect()
    }

    fn plain() -> TermCaps {
        TermCaps {
            tty: false,
            color: ColorLevel::None,
            charset: Charset::Unicode,
            width: 80,
        }
    }

    #[test]
    fn choosing_a_mode_is_not_consent_to_system_integrations() {
        for flags in [
            &["--local"][..],
            &["--connect"][..],
            &["--force"][..],
            &["--local", "--force"][..],
            &["--connect", "--telemetry"][..],
        ] {
            let parsed = OnboardFlags::parse(&args(flags));
            assert!(!parsed.assume_yes, "{flags:?} must not imply consent");
            assert_eq!(
                integration_decision(parsed.assume_yes, false),
                IntegrationDecision::Skip,
                "non-interactive {flags:?} must leave integrations off"
            );
            assert_eq!(
                integration_decision(parsed.assume_yes, true),
                IntegrationDecision::Ask,
                "interactive {flags:?} must ask"
            );
        }
    }

    #[test]
    fn explicit_yes_applies_system_integrations() {
        for flags in [&["--local", "--yes"][..], &["--connect", "-y"][..]] {
            let parsed = OnboardFlags::parse(&args(flags));
            assert!(parsed.assume_yes);
            assert_eq!(
                integration_decision(parsed.assume_yes, false),
                IntegrationDecision::Apply
            );
        }
    }

    #[test]
    fn clean_setup_ends_with_all_set() {
        let text = render_finished(false, &SetupReport::default(), &plain());
        assert!(text.contains("You're all set!"), "{text}");
        assert!(!text.contains("Set up with warnings"), "{text}");
        assert!(!text.contains("autter dash"), "{text}");
        assert!(render_finished(true, &SetupReport::default(), &plain()).contains("autter dash"));
    }

    #[test]
    fn degraded_setup_lists_each_problem_with_its_fix_instead_of_all_set() {
        let mut report = SetupReport::default();
        report.problem(
            "Could not restart the background service (timed out)",
            crate::commands::daemon::RESTART_COMMAND,
        );
        report.problem(
            "Could not install hooks (denied)",
            "autter install --system",
        );
        let text = render_finished(true, &report, &plain());
        assert!(!text.contains("You're all set"), "{text}");
        assert!(text.contains("Set up with warnings"), "{text}");
        assert!(text.contains("2 steps did not complete"), "{text}");
        assert!(
            text.contains("Could not restart the background service (timed out)"),
            "{text}"
        );
        // The daemon fix is the canonical restart command.
        assert!(text.contains("Fix: autter bg restart"), "{text}");
        assert!(text.contains("Fix: autter install --system"), "{text}");
    }

    #[test]
    fn multi_line_problems_stay_on_the_rail() {
        let mut report = SetupReport::default();
        report.problem(
            "Could not connect (CLI authorization failed: denied\nNo credentials were issued or changed.)",
            "autter onboard --connect",
        );
        let text = render_finished(false, &report, &plain());
        assert!(
            text.contains(
                "│  • Could not connect (CLI authorization failed: denied\n\
                 │    No credentials were issued or changed.)\n\
                 │    Fix: autter onboard --connect\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn steps_count_only_the_questions_asked() {
        let plan = StepPlan {
            ask_mode: true,
            sign_in: true,
            ask_telemetry: true,
            ask_integrations: true,
            maybe_backlog: false,
        };
        assert_eq!(plan.total(true), 4);
        assert_eq!(plan.total(false), 3);

        let flags_given = StepPlan {
            ask_mode: false,
            sign_in: false,
            ask_telemetry: false,
            ask_integrations: true,
            maybe_backlog: true,
        };
        assert_eq!(flags_given.total(true), 2);
        assert_eq!(flags_given.total(false), 1);
    }

    #[test]
    fn step_numbers_follow_the_chosen_path() {
        let mut steps = Steps::new(true, 4);
        assert_eq!(steps.next().as_deref(), Some("1/4"));
        steps.set_total(3);
        assert_eq!(steps.next().as_deref(), Some("2/3"));
        assert_eq!(steps.next().as_deref(), Some("3/3"));

        let mut quiet = Steps::new(false, 4);
        assert_eq!(quiet.next(), None);
    }

    #[test]
    fn help_flag_is_detected_in_any_position() {
        assert!(is_help_request(&args(&["--help"])));
        assert!(is_help_request(&args(&["-h"])));
        assert!(is_help_request(&args(&["help"])));
        assert!(is_help_request(&args(&["--connect", "--help"])));
        assert!(!is_help_request(&args(&[])));
        assert!(!is_help_request(&args(&["--connect"])));
        assert!(!is_help_request(&args(&["--local", "--force"])));
        assert!(!is_help_request(&args(&["--helpful"])));
    }
}
