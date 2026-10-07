//! `autter sync` — inspect cloud upload health and local queue depth.
//!
//! Subcommands:
//! - `status`  Show whether authorship data is reaching autter cloud.
//! - `purge`   Discard the local upload backlog without uploading it.
//! - `open`    Open the org dashboard.

use crate::auth::notice::{CloudSyncState, clear_sync_auth_blocked, collect_cloud_sync_status};
use crate::commands::arg_parser::{self, ScanMode};
use crate::error::AutterError;
use serde::Serialize;
use std::io::IsTerminal;

#[derive(Debug, Serialize)]
struct SyncStatusOutput {
    cloud_sync: crate::auth::notice::CloudSyncStatusReport,
    ok: bool,
}

pub fn handle_sync(args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        None => print_status(args),
        Some("status") => print_status(&args[1..]),
        Some("purge") | Some("clear") | Some("clear-queue") => purge_cli(&args[1..]),
        Some("import") => import_cli(&args[1..]),
        Some("open") => open_dashboard(),
        Some("--help") | Some("-h") | Some("help") => print_help(),
        Some(other) => {
            eprintln!("Unknown sync subcommand: {other}");
            print_help();
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    }
}

fn purge_cli(args: &[String]) {
    let force = args
        .iter()
        .any(|a| a == "--force" || a == "-f" || a == "--yes" || a == "-y");
    if !force && std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        eprintln!("This deletes local cloud-upload queues (notes, transcripts, metrics, …).");
        eprintln!("It does not delete git notes already written to refs/notes/ai.");
        eprintln!("Re-run with --force to confirm.");
        std::process::exit(crate::commands::EXIT_USAGE_ERROR);
    }

    match purge_sync_queues() {
        Ok(summary) => {
            println!("Cleared local upload queue: {summary}");
        }
        Err(e) => {
            eprintln!("Failed to clear upload queue: {e}");
            std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
        }
    }
}

/// `autter sync import`: upload the held backlog (explicit consent).
fn import_cli(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }
    if let Err(e) = crate::upload_hold::ensure_legacy_migration() {
        eprintln!("Could not read the upload hold: {e}");
        std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
    }
    let held = crate::upload_hold::held_counts();
    if held.total() == 0 {
        println!("Nothing is held; there is no backlog waiting for your consent.");
        return;
    }
    if !crate::config::platform_sync_enabled_now() {
        eprintln!(
            "This machine is in local mode, so nothing is uploaded. Connect first with `autter onboard --connect`."
        );
        std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
    }
    let consent = args.iter().any(|a| a == "--yes" || a == "-y");
    if !consent {
        println!("Held for your consent: {}.", held.summary());
        println!("These were queued before you connected this machine, or by an older autter");
        println!("version, and may include transcripts recorded in local mode or from");
        println!("repositories excluded from prompt sharing.");
        println!();
        println!("Upload them:  {}", crate::upload_hold::IMPORT_COMMAND);
        println!("Delete them:  {}", crate::upload_hold::PURGE_COMMAND);
        return;
    }
    match crate::upload_hold::release_all() {
        Ok(released) => {
            println!(
                "Released for upload: {}. The background service uploads them shortly.",
                released.summary()
            );
        }
        Err(e) => {
            eprintln!("Failed to release the held backlog: {e}");
            std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
        }
    }
}

/// Discard every durable cloud-sync queue on this machine, held or not.
pub fn purge_sync_queues() -> Result<String, AutterError> {
    let mut parts = Vec::new();

    if let Ok(db) = crate::metrics::db::MetricsDatabase::global()
        && let Ok(mut guard) = db.lock()
    {
        let n = guard.delete_all().unwrap_or(0);
        if n > 0 {
            parts.push(format!("{n} telemetry events"));
        }
    }
    if let Ok(db) = crate::notes::db::NotesDatabase::global()
        && let Ok(mut guard) = db.lock()
    {
        let notes = guard.delete_pending().unwrap_or(0);
        let summaries = guard.delete_pending_commit_summaries().unwrap_or(0);
        if notes > 0 {
            parts.push(format!("{notes} authorship notes"));
        }
        if summaries > 0 {
            parts.push(format!("{summaries} commit summaries"));
        }
    }
    if let Ok(db) = crate::authorship::internal_db::InternalDatabase::global()
        && let Ok(mut guard) = db.lock()
    {
        let n = guard.delete_pending_cas().unwrap_or(0);
        if n > 0 {
            parts.push(format!("{n} transcripts"));
        }
    }
    if let Ok(db) = crate::file_changes::FileChangesDatabase::global()
        && let Ok(mut guard) = db.lock()
    {
        let n = guard.delete_pending().unwrap_or(0);
        if n > 0 {
            parts.push(format!("{n} file-change records"));
        }
    }

    clear_sync_auth_blocked();
    crate::auth::notice::clear_upload_failures();

    Ok(if parts.is_empty() {
        "nothing pending".to_string()
    } else {
        parts.join(", ")
    })
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
    eprintln!("  autter sync purge --force");
    eprintln!("  autter sync import --yes");
    eprintln!("  autter sync open");
    eprintln!();
    eprintln!("Shows whether authorship data is reaching autter cloud, how much is");
    eprintln!("queued locally, and what to do when uploads are blocked.");
    eprintln!();
    eprintln!("`purge` discards the local upload backlog without uploading it.");
    eprintln!("Use this before `autter login` if you do not want historical sessions uploaded.");
    eprintln!();
    eprintln!("`import` uploads the held backlog: records queued before you connected this");
    eprintln!("machine, or by an older autter version. Held records never upload on their own.");
    eprintln!("Without --yes it only shows what is held.");
    eprintln!();
    eprintln!("Status exits 0 when upload is off or no problem is detected.");
    eprintln!("It exits 1 when sign-in, upload, the service, or a queue read needs attention.");
}
