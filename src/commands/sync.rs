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
        None => print_status(args),
        Some("status") => print_status(&args[1..]),
        Some("open") => open_dashboard(),
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
    println!("{}", crate::auth::notice::format_sync_report(report));
    if !report.enabled {
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

    if !report.queue_status_available {
        println!("Pending uploads: unavailable");
    } else if report.pending.total > 0 {
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
        println!("Records are waiting for upload. Run `autter sync status` again to check");
        println!("the queue, or `autter doctor` if the count does not decrease.");
    } else {
        println!();
        println!("An empty queue does not confirm that every local change is on the dashboard.");
    }
    if let Some(dashboard_url) = report.dashboard_url.as_deref() {
        println!("Dashboard: {dashboard_url}");
    }
}

fn open_dashboard() {
    let report = collect_cloud_sync_status();
    let Some(dashboard_url) = report.dashboard_url else {
        eprintln!("Dashboard organization unavailable. Run `autter login`, then `autter whoami`.");
        std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
    };
    println!("Dashboard: {dashboard_url}");
    if crate::commands::personal_dashboard::open_browser(&dashboard_url).is_err() {
        eprintln!("Could not open the browser. Open the dashboard address shown above.");
        std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
    }
}

fn state_label(state: CloudSyncState) -> &'static str {
    match state {
        CloudSyncState::Disabled => "disabled",
        CloudSyncState::Healthy => "no queued uploads",
        CloudSyncState::Draining => "upload pending",
        CloudSyncState::AuthBlocked => "auth blocked",
        CloudSyncState::UploadFailing => "upload failing",
        CloudSyncState::DaemonNotRunning => "background service not running",
        CloudSyncState::StatusUnavailable => "queue status unavailable",
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
    eprintln!("  autter sync open");
    eprintln!();
    eprintln!("Shows whether authorship data is reaching autter cloud, how much is");
    eprintln!("queued locally, and what to do when uploads are blocked.");
    eprintln!();
    eprintln!("Status exits 0 when upload is off or no problem is detected.");
    eprintln!("It exits 1 when sign-in, upload, the service, or a queue read needs attention.");
}
