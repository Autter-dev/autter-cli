//! First-run onboarding flow (`autter onboard`).
//!
//! After installation, autter walks the user through a one-time guided setup:
//! an arrow-key choice between the two modes, a telemetry consent choice, and
//! a "you're all set" summary with the first commands to try.
//!
//! - **Local mode** — everything stays on the machine (git notes + local
//!   storage). Nothing is uploaded. The user is told they will not get the
//!   platform's detailed prompt-usage and team/user-usage dashboards.
//!
//! - **Connected mode** — the user links their Autter account (OAuth device
//!   flow). Attribution is still written to local git notes exactly as before,
//!   and prompt/usage data additionally syncs to the org's Autter platform
//!   (persisted in the org's PostgreSQL via the platform API).
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
//! Note: the full authorship-note → Postgres dual-write (keeping local git
//! notes while also uploading the note itself) is delivered alongside the
//! in-repo backend. Today, connected mode keeps local git notes and uploads
//! prompt/usage data via the existing CAS upload path.

use std::collections::BTreeMap;
use std::io::IsTerminal;

use serde_json::json;

use crate::auth::{AuthState, collect_auth_status};
use crate::commands::login::run_device_login;
use crate::config::{self, NotesBackendConfig, NotesBackendKind};
use crate::ui::{self, BOLD, CYAN, DIM, GREEN, RESET, SelectItem};

/// Entry point for the `autter onboard` command.
pub fn handle_onboard(args: &[String]) {
    let force = args.iter().any(|a| a == "--force" || a == "-f");
    let choose_connect = args.iter().any(|a| a == "--connect");
    let choose_local = args.iter().any(|a| a == "--local");
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

    print_welcome();

    let logged_in = matches!(collect_auth_status().state, AuthState::LoggedIn);

    let connect = if choose_connect {
        true
    } else if choose_local {
        false
    } else if logged_in {
        // Already authenticated (e.g. via an install nonce): treat as connected
        // without prompting.
        true
    } else if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        prompt_mode_choice()
    } else {
        // Non-interactive shell with no explicit choice: don't block automated
        // installs and don't guess — leave onboarding incomplete so the user can
        // run `autter onboard` later from a real terminal.
        eprintln!("Non-interactive shell detected — skipping onboarding for now.");
        eprintln!("Run `autter onboard` from your terminal to choose local or connected mode.");
        return;
    };

    if connect {
        setup_connected(&mut file_config, logged_in);
    } else {
        setup_local(&mut file_config);
    }

    // Telemetry consent is asked regardless of the connected/local choice above.
    let telemetry_enabled = configure_telemetry(&mut file_config, telemetry_flag);

    file_config.onboarding_completed = Some(true);
    if let Err(e) = config::save_file_config(&file_config) {
        eprintln!("Warning: could not save onboarding state: {e}");
    }

    // Reflect the mode actually configured: a connect attempt can fall back
    // to local if the device login fails.
    let connected = file_config.prompt_storage.as_deref() != Some("local");
    print_finished(connected);

    // Fire a one-off install/onboard event so we can count installs by version
    // and platform. Only when the user just opted in.
    if telemetry_enabled {
        record_install_event(connected);
    }
}

/// The interactive connected-vs-local choice, as an arrow-key selector.
fn prompt_mode_choice() -> bool {
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
    ui::select("How should Autter run on this machine?", &items, 0) == 0
}

/// Ask whether to enable anonymous telemetry + error tracking and persist the
/// choice into `telemetry_oss` ("on"/"off"). Returns whether telemetry ended up
/// enabled.
fn configure_telemetry(cfg: &mut config::FileConfig, flag: Option<bool>) -> bool {
    let local_log = config::id_file_path()
        .and_then(|p| p.parent().map(|d| d.join("telemetry.log")))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.autter/internal/telemetry.log".to_string());

    eprintln!();
    eprintln!("  {DIM}One last thing — anonymous usage analytics and error reporting.");
    eprintln!("  No personal data is ever collected: no code, prompts, file paths,");
    eprintln!("  repo names, usernames, or IP addresses. Only a random install ID,");
    eprintln!("  coarse device info (OS, CPU architecture, core count) and the");
    eprintln!("  Autter version. Everything sent is mirrored to a local log:");
    eprintln!("      {local_log}{RESET}");
    eprintln!();

    let enabled = match flag {
        Some(value) => value,
        None => {
            if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
                let items = [
                    SelectItem::new(
                        "Enable — help improve Autter",
                        "Audit every event in the local log, or turn it off anytime with `autter telemetry off`.",
                    ),
                    SelectItem::new(
                        "Disable — send nothing",
                        "You can opt in later with `autter telemetry on`.",
                    ),
                ];
                ui::select("Enable anonymous telemetry and error tracking?", &items, 0) == 0
            } else {
                // Non-interactive and no explicit flag: default to disabled so we
                // never collect data the user didn't actively agree to.
                false
            }
        }
    };

    cfg.telemetry_oss = Some(if enabled { "on" } else { "off" }.to_string());

    eprintln!();
    if enabled {
        eprintln!(
            "  {GREEN}\u{2713}{RESET} Telemetry enabled. Thank you — you can review what's sent at:"
        );
        eprintln!("      {DIM}{local_log}{RESET}");
    } else {
        eprintln!(
            "  {GREEN}\u{2713}{RESET} Telemetry disabled. Nothing will be collected or sent."
        );
    }

    enabled
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

/// Configure local-only mode and explain the trade-off.
fn setup_local(cfg: &mut config::FileConfig) {
    // Keep everything on the machine: prompts only in local SQLite, attribution
    // in local git notes, nothing uploaded.
    cfg.prompt_storage = Some("local".to_string());
    cfg.notes_backend = Some(NotesBackendConfig {
        kind: NotesBackendKind::GitNotes,
        backend_url: None,
    });

    eprintln!();
    eprintln!("{GREEN}{BOLD}\u{2713} Autter is set up in local mode.{RESET}");
    eprintln!();
    eprintln!("  \u{2022} Attribution, blame, and stats run entirely on your machine.");
    eprintln!("  \u{2022} Data stays in this repo's git notes (refs/notes/ai) and local storage.");
    eprintln!("  \u{2022} Nothing is uploaded to the Autter platform.");
    eprintln!();
    eprintln!("  {DIM}Heads up: in local mode you will NOT have access to the Autter platform's");
    eprintln!("  detailed prompt usage and team/user usage dashboards.");
    eprintln!();
    eprintln!("  Run `autter onboard --connect` anytime to link your Autter account.{RESET}");
}

/// Log the user in (if needed) and configure connected mode.
fn setup_connected(cfg: &mut config::FileConfig, already_logged_in: bool) {
    if !already_logged_in {
        eprintln!();
        eprintln!("Connecting to the Autter platform...");
        if let Err(e) = run_device_login() {
            eprintln!();
            eprintln!("\u{2717} Could not connect: {e}");
            eprintln!(
                "  Setting up local mode for now \u{2014} run `autter onboard --connect` to retry."
            );
            setup_local(cfg);
            return;
        }
    }

    // Connected mode: upload prompts (CAS) and authorship notes to the hosted
    // data plane. `backend_url: None` resolves to DEFAULT_NOTES_BACKEND_URL
    // (cli.autter.dev) via Config::notes_backend_url().
    cfg.prompt_storage = Some("default".to_string());
    cfg.notes_backend = Some(NotesBackendConfig {
        kind: NotesBackendKind::Http,
        backend_url: None,
    });

    let status = collect_auth_status();
    let who = status
        .email
        .or(status.name)
        .unwrap_or_else(|| "your Autter account".to_string());

    eprintln!();
    eprintln!("{GREEN}{BOLD}\u{2713} Connected to the Autter platform as {who}.{RESET}");
    eprintln!();
    eprintln!("  \u{2022} Attribution is still written locally to git notes (refs/notes/ai).");
    eprintln!("  \u{2022} Prompt and usage data also sync to your org's Autter dashboard,");
    eprintln!("    so you get detailed prompt usage and team/user analytics.");
    eprintln!();
    eprintln!("  {DIM}Manage your account with `autter whoami` / `autter logout`.{RESET}");
}

fn print_welcome() {
    let version = env!("CARGO_PKG_VERSION");
    eprintln!();
    eprintln!("  {BOLD}Welcome to Autter{RESET} \u{1F9A6}  {DIM}v{version}{RESET}");
    eprintln!();
    eprintln!("  Autter records which lines of your code were written by AI \u{2014} tied to");
    eprintln!("  the agent, model, and prompts behind them \u{2014} right in Git itself.");
    eprintln!();
    eprintln!("  {DIM}Setup takes about a minute: two quick choices and you're done.{RESET}");
    eprintln!();
}

/// Closing summary: confirmation that nothing else is required, plus the first
/// commands worth trying.
fn print_finished(connected: bool) {
    eprintln!();
    eprintln!("{GREEN}{BOLD}\u{2713} You're all set!{RESET}");
    eprintln!();
    eprintln!("  Just keep coding \u{2014} authorship is recorded automatically on every commit.");
    eprintln!();
    eprintln!("  Try it on any repo:");
    eprintln!(
        "    {CYAN}autter stats{RESET}          {DIM}AI vs human share of your latest commit{RESET}"
    );
    eprintln!(
        "    {CYAN}autter blame <file>{RESET}   {DIM}who \u{2014} you or an agent \u{2014} wrote each line{RESET}"
    );
    eprintln!(
        "    {CYAN}autter log{RESET}            {DIM}git log with AI authorship stats{RESET}"
    );
    if connected {
        eprintln!(
            "    {CYAN}autter dash{RESET}           {DIM}open your personal dashboard{RESET}"
        );
    }
    eprintln!();
    eprintln!("  {DIM}All commands: `autter help` \u{2022} Docs: https://autter.dev/docs{RESET}");
}

fn print_status_summary() {
    let status = collect_auth_status();
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
        let local_log = config::id_file_path()
            .and_then(|p| p.parent().map(|d| d.join("telemetry.log")))
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.autter/internal/telemetry.log".to_string());
        eprintln!("Anonymous telemetry is ON. Everything sent is logged at:");
        eprintln!("  {local_log}");
    } else {
        eprintln!("Anonymous telemetry is OFF.");
    }
}
