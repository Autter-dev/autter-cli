//! First-run onboarding flow (`autter onboard`).
//!
//! After installation, autter walks the user through a one-time choice:
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
//! The chosen mode is recorded in `~/.autter/config.json` via
//! `onboarding_completed`, so re-running an installer does not nag the user.
//!
//! Note: the full authorship-note → Postgres dual-write (keeping local git
//! notes while also uploading the note itself) is delivered alongside the
//! in-repo backend. Today, connected mode keeps local git notes and uploads
//! prompt/usage data via the existing CAS upload path.

use std::io::{IsTerminal, Write};

use crate::auth::{AuthState, collect_auth_status};
use crate::commands::login::run_device_login;
use crate::config::{self, NotesBackendConfig, NotesBackendKind};

/// Entry point for the `autter onboard` command.
pub fn handle_onboard(args: &[String]) {
    let force = args.iter().any(|a| a == "--force" || a == "-f");
    let choose_connect = args.iter().any(|a| a == "--connect");
    let choose_local = args.iter().any(|a| a == "--local");

    let mut file_config = config::load_file_config_public().unwrap_or_default();

    // Already onboarded and no explicit override: just show status and exit.
    if file_config.onboarding_completed == Some(true) && !force && !choose_connect && !choose_local
    {
        print_status_summary();
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
        prompt_yes_no("Connect this machine to the Autter platform?", false)
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

    file_config.onboarding_completed = Some(true);
    if let Err(e) = config::save_file_config(&file_config) {
        eprintln!("Warning: could not save onboarding state: {e}");
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
    eprintln!("\u{2713} Autter is set up in local mode.");
    eprintln!();
    eprintln!("  \u{2022} Attribution, blame, and stats run entirely on your machine.");
    eprintln!("  \u{2022} Data stays in this repo's git notes (refs/notes/ai) and local storage.");
    eprintln!("  \u{2022} Nothing is uploaded to the Autter platform.");
    eprintln!();
    eprintln!("  Heads up: in local mode you will NOT have access to the Autter platform's");
    eprintln!("  detailed prompt usage and team/user usage dashboards.");
    eprintln!();
    eprintln!("  Run `autter onboard --connect` anytime to link your Autter account.");
}

/// Log the user in (if needed) and configure connected mode.
fn setup_connected(cfg: &mut config::FileConfig, already_logged_in: bool) {
    if !already_logged_in {
        eprintln!();
        eprintln!("Connecting to the Autter platform...");
        if let Err(e) = run_device_login() {
            eprintln!();
            eprintln!("\u{2717} Could not connect: {e}");
            eprintln!("  Setting up local mode for now \u{2014} run `autter onboard --connect` to retry.");
            setup_local(cfg);
            return;
        }
    }

    // Upload prompt/usage data to the platform while keeping local git notes.
    cfg.prompt_storage = Some("default".to_string());
    cfg.notes_backend = Some(NotesBackendConfig {
        kind: NotesBackendKind::GitNotes,
        backend_url: None,
    });

    let status = collect_auth_status();
    let who = status
        .email
        .or(status.name)
        .unwrap_or_else(|| "your Autter account".to_string());

    eprintln!();
    eprintln!("\u{2713} Connected to the Autter platform as {who}.");
    eprintln!();
    eprintln!("  \u{2022} Attribution is still written locally to git notes (refs/notes/ai).");
    eprintln!("  \u{2022} Prompt and usage data also sync to your org's Autter dashboard,");
    eprintln!("    so you get detailed prompt usage and team/user analytics.");
    eprintln!();
    eprintln!("  Manage your account with `autter whoami` / `autter logout`.");
}

fn print_welcome() {
    eprintln!();
    eprintln!("  Welcome to Autter \u{1F9A6}");
    eprintln!();
    eprintln!("  Autter tracks which lines of code were written by AI agents and links");
    eprintln!("  them to the agent, model, and prompts behind them.");
    eprintln!();
    eprintln!("  You can connect this machine to the Autter platform for detailed prompt");
    eprintln!("  usage and team/user analytics, or run fully local \u{2014} your choice.");
    eprintln!();
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
    }
}

/// Prompt a yes/no question on stderr and read the answer from stdin.
fn prompt_yes_no(question: &str, default_yes: bool) -> bool {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    eprint!("{question} {hint} ");
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return default_yes;
    }

    match input.trim().to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default_yes,
    }
}
