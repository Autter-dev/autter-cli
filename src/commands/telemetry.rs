//! `autter telemetry` — inspect and control anonymous telemetry.
//!
//! Subcommands:
//! - `status`           Show whether telemetry is on/off and where the audit log lives.
//! - `log [-n N|--all]` Print the local audit log of everything sent to PostHog.
//! - `enable` / `on`    Turn telemetry on (persisted to `~/.autter/config.json`).
//! - `disable` / `off`  Turn telemetry off.
//!
//! The audit log is written by [`crate::telemetry_client`]; this command is a
//! read-only/settings front-end so users never have to re-run onboarding just
//! to check or change their choice.

use crate::config;

pub fn handle_telemetry(args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        None | Some("status") => print_status(),
        Some("log") | Some("logs") => print_log(&args[1..]),
        Some("enable") | Some("on") => set_enabled(true),
        Some("disable") | Some("off") => set_enabled(false),
        Some("--help") | Some("-h") | Some("help") => print_help(),
        Some(other) => {
            eprintln!("Unknown telemetry subcommand: {other}");
            print_help();
            std::process::exit(1);
        }
    }
}

fn log_path() -> Option<std::path::PathBuf> {
    crate::telemetry_client::local_log_path()
}

fn is_enabled() -> bool {
    // Reads the persisted choice directly so it reflects edits made outside this
    // process. Telemetry is on unless explicitly set to "off".
    config::load_file_config_public()
        .ok()
        .and_then(|c| c.telemetry_oss)
        .as_deref()
        != Some("off")
}

fn print_status() {
    let enabled = is_enabled();
    println!(
        "Anonymous telemetry: {}",
        if enabled { "ON" } else { "OFF" }
    );
    println!(
        "Install ID:          {}",
        config::get_or_create_distinct_id()
    );
    if let Some(path) = log_path() {
        println!("Audit log:           {}", path.display());
    }
    println!();
    if enabled {
        println!("Only anonymous data is sent (OS, CPU arch, core count, version, random");
        println!("install ID). Run `autter telemetry log` to see exactly what was sent,");
        println!("or `autter telemetry off` to disable.");
    } else {
        println!("Nothing is collected or sent. Run `autter telemetry on` to enable.");
    }
}

fn print_log(args: &[String]) {
    let Some(path) = log_path() else {
        eprintln!("Could not determine telemetry log path.");
        return;
    };

    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            println!(
                "No telemetry has been sent yet (no log at {}).",
                path.display()
            );
            return;
        }
    };

    let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();

    let show_all = args.iter().any(|a| a == "--all");
    let count: usize = if show_all {
        lines.len()
    } else {
        args.iter()
            .position(|a| a == "-n" || a == "--lines")
            .and_then(|i| args.get(i + 1))
            .and_then(|n| n.parse().ok())
            .unwrap_or(50)
    };

    let start = lines.len().saturating_sub(count);
    for line in &lines[start..] {
        println!("{line}");
    }
}

fn set_enabled(enabled: bool) {
    let mut cfg = config::load_file_config_public().unwrap_or_default();
    cfg.telemetry_oss = Some(if enabled { "on" } else { "off" }.to_string());
    match config::save_file_config(&cfg) {
        Ok(()) => {
            if enabled {
                println!("\u{2713} Anonymous telemetry enabled.");
                if let Some(path) = log_path() {
                    println!("  Review what's sent anytime: {}", path.display());
                }
            } else {
                println!(
                    "\u{2713} Anonymous telemetry disabled. Nothing will be collected or sent."
                );
            }
        }
        Err(e) => {
            eprintln!("Failed to update config: {e}");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    eprintln!("Usage: autter telemetry <command>");
    eprintln!();
    eprintln!("Commands:");
    eprintln!(
        "  status            Show whether telemetry is on/off and the audit log path (default)"
    );
    eprintln!("  log [-n N|--all]  Print the local audit log of data sent (default: last 50)");
    eprintln!("  enable, on        Turn anonymous telemetry on");
    eprintln!("  disable, off      Turn anonymous telemetry off");
}
