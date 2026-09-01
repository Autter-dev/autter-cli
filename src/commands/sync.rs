//! `autter sync` — inspect cloud upload health and local queue depth.
//!
//! Subcommands:
//! - `status`  Show whether authorship data is reaching autter cloud.

use crate::auth::notice::{CloudSyncState, collect_cloud_sync_status};
use crate::commands::arg_parser::{self, ScanMode};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct SyncStatusOutput {
    cloud_sync: crate::auth::notice::CloudSyncStatusReport,
    ok: bool,
}

pub fn handle_sync(args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        None | Some("status") => print_status(&args[1..]),
        Some("--help") | Some("-h") | Some("help") => print_help(),
        Some(other) => {
            eprintln!("Unknown sync subcommand: {other}");
            print_help();
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    }
}

fn print_status(args: &[String]) {
    let pp = match arg_parser::pre_parse(args, ScanMode::Full, false) {
        Ok(pp) => pp,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    };
    if pp.flags.help {
        print_help();
        return;
    }
    arg_parser::merge_global_flags(&pp.flags);

    let report = collect_cloud_sync_status();
    let ok = !report.enabled
        || matches!(
            report.state,
            CloudSyncState::Healthy | CloudSyncState::Draining
        );

    if arg_parser::json() {
        let output = SyncStatusOutput {
            cloud_sync: report,
            ok,
        };
        println!(
            "{}",
            serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        print_human_status(&report);
    }

    if !ok {
        std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
    }
}

fn print_human_status(report: &crate::auth::notice::CloudSyncStatusReport) {
    if !report.enabled {
        println!("Cloud sync: disabled (local-only mode)");
        println!();
        println!("Authorship data stays on this machine. Run `autter onboard` or");
        println!("`autter config set notes_backend.kind http` to enable cloud upload.");
        return;
    }

    println!("Cloud sync: enabled ({})", state_label(report.state));
    println!(
        "Background service: {}",
        if report.daemon_running {
            "running"
        } else {
            "not running"
        }
    );

    if report.pending.total > 0 {
        println!("Pending uploads:");
        print_pending_line("  telemetry events", report.pending.metrics);
        print_pending_line("  authorship notes", report.pending.notes);
        print_pending_line("  commit summaries", report.pending.commit_summaries);
        print_pending_line("  transcripts", report.pending.transcripts);
        print_pending_line("  file-change records", report.pending.file_changes);
    } else {
        println!("Pending uploads: none");
    }

    if let Some(remediation) = &report.remediation {
        println!();
        println!("Fix: {remediation}");
    } else if report.state == CloudSyncState::Draining {
        println!();
        println!("Uploads are in progress. Re-run `autter sync status` to confirm the");
        println!("queue drains, or `autter doctor` if counts stop decreasing.");
    } else {
        println!();
        println!("Cloud uploads are healthy.");
    }
}

fn state_label(state: CloudSyncState) -> &'static str {
    match state {
        CloudSyncState::Disabled => "disabled",
        CloudSyncState::Healthy => "healthy",
        CloudSyncState::Draining => "draining backlog",
        CloudSyncState::AuthBlocked => "auth blocked",
        CloudSyncState::UploadFailing => "upload failing",
        CloudSyncState::DaemonNotRunning => "background service not running",
    }
}

fn print_pending_line(label: &str, count: i64) {
    if count > 0 {
        println!("{label}: {count}");
    }
}

fn print_help() {
    eprintln!("autter sync - inspect cloud upload health");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  autter sync status [--json]");
    eprintln!();
    eprintln!("Shows whether authorship data is reaching autter cloud, how much is");
    eprintln!("queued locally, and what to do when uploads are blocked.");
    eprintln!();
    eprintln!("Exits 0 when cloud sync is disabled, healthy, or actively draining a");
    eprintln!("backlog. Exits 1 when user action is required.");
}
