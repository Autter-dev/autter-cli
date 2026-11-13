//! `autter verify [<rev|range>] [--json] [--strict] [--no-record]
//! [--require-record]`: the integrity check of authorship notes (see
//! `authorship::note_integrity`), and in connected mode their comparison
//! with the server's record (see `authorship::note_server_record`).
//!
//! Exit codes: 0 when no note fails; [`EXIT_INTEGRITY_FAILED`] (5) when a note
//! belongs to another commit, was changed (locally detected, or against the
//! server's record: `changed_after_recorded`), or was carried by a rewrite
//! from a note that failed. With `--strict`, a note rebound by an older
//! autter (whose digest cannot be checked) also exits 5. Missing and legacy
//! notes are warnings, also with `--strict` for now (a later release will
//! fail them under `--strict`). `not_recorded` is a warning, also with
//! `--strict`; with `--require-record` it exits 5, and so does a run whose
//! server record could not be checked at all (local mode, not logged in,
//! server unreachable or too old).

use crate::authorship::note_integrity::{self, CommitIntegrity, IntegrityStatus};
use crate::authorship::note_server_record::{self, CompareMode, RecordComparison};
use crate::commands::arg_parser::{self, ScanMode};
use crate::commands::output::{self, ErrorKind};
use crate::error::AutterError;
use crate::git::find_repository;
use crate::git::repository::{Repository, exec_git};
use serde::Serialize;
use std::collections::BTreeMap;

pub use crate::commands::output::EXIT_INTEGRITY_FAILED;

#[derive(Serialize)]
struct VerifyReport<'a> {
    range: &'a str,
    commits: usize,
    counts: BTreeMap<&'static str, usize>,
    failed: usize,
    results: &'a [VerifyResult],
    record_check: &'a RecordCheck,
}

/// One commit's result: the integrity check, and how its note compares with
/// the server's record when that was checked.
#[derive(Serialize)]
struct VerifyResult {
    #[serde(flatten)]
    integrity: CommitIntegrity,
    #[serde(flatten)]
    record: Option<RecordComparison>,
}

/// Whether the server's record was checked, and if not, why.
#[derive(Serialize)]
struct RecordCheck {
    checked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<CompareMode>,
    required: bool,
}

#[derive(Clone, Copy)]
struct Policy {
    strict: bool,
    require_record: bool,
}

pub fn handle_verify(args: &[String]) {
    let pp = match arg_parser::pre_parse(args, ScanMode::Full, false) {
        Ok(p) => p,
        Err(e) => output::fail(ErrorKind::Usage, format!("error: {}", e)),
    };
    if pp.flags.help {
        arg_parser::print_command_help("verify");
        return;
    }
    arg_parser::merge_global_flags(&pp.flags);
    let mut target: Option<String> = None;
    let mut no_record = false;
    let mut require_record = false;
    for arg in &pp.rest {
        match arg.as_str() {
            "--no-record" => {
                no_record = true;
                continue;
            }
            "--require-record" => {
                require_record = true;
                continue;
            }
            _ => {}
        }
        if arg.starts_with('-') {
            arg_parser::exit_unknown_option("verify", arg);
        }
        if target.is_some() {
            output::fail(
                ErrorKind::Usage,
                "error: autter verify takes one revision or range",
            );
        }
        target = Some(arg.clone());
    }
    if no_record && require_record {
        output::fail(
            ErrorKind::Usage,
            "error: --no-record and --require-record cannot be used together",
        );
    }
    let target = target.unwrap_or_else(|| "HEAD".to_string());

    let repo = match find_repository(&[]) {
        Ok(repo) => repo,
        Err(e) => crate::commands::exit_repository_error("verify", &e),
    };
    let commits = match resolve_commits(&repo, &target) {
        Ok(commits) => commits,
        Err(_) if crate::commands::repository_has_no_commits(&repo) => output::fail(
            ErrorKind::Runtime,
            crate::commands::no_commits_message("verify"),
        ),
        Err(e) => output::fail(
            output::error_kind(&e),
            format!("autter verify: cannot resolve {target}: {e}"),
        ),
    };
    let notes = match crate::git::notes_api::read_notes_batch(&repo, &commits) {
        Ok(notes) => notes,
        Err(e) => output::fail(ErrorKind::Runtime, format!("autter verify: {e}")),
    };
    let integrity = match note_integrity::verify_notes(&repo, &commits, &notes) {
        Ok(results) => results,
        Err(e) => output::fail(ErrorKind::Runtime, format!("autter verify: {e}")),
    };

    // Compare with the server's record, when there is one to compare with.
    let fetched = if no_record {
        Err("--no-record".to_string())
    } else if commits.is_empty() {
        Err("no commits to check".to_string())
    } else {
        note_server_record::fetch_records(&commits)
    };
    let record_check = RecordCheck {
        checked: fetched.is_ok(),
        reason: fetched.as_ref().err().cloned(),
        mode: fetched.as_ref().ok().map(|f| f.mode),
        required: require_record,
    };
    let results: Vec<VerifyResult> = integrity
        .into_iter()
        .map(|mut r| {
            let commit = r.commit.clone();
            let record = fetched.as_ref().ok().and_then(|f| {
                note_server_record::apply_record(
                    &mut r,
                    notes.get(&commit).map(String::as_str),
                    f.records.get(&commit),
                    f.mode,
                )
            });
            VerifyResult {
                integrity: r,
                record,
            }
        })
        .collect();

    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for r in &results {
        *counts.entry(r.integrity.status.as_str()).or_default() += 1;
    }
    let policy = Policy {
        strict: output::strict(),
        require_record,
    };
    let failed = results
        .iter()
        .filter(|r| fails(r.integrity.status, policy))
        .count();
    // --require-record: a range whose record could not be checked fails.
    let record_missing = require_record && !record_check.checked && !results.is_empty();
    let unproven = results
        .iter()
        .filter(|r| {
            matches!(
                r.integrity.status,
                IntegrityStatus::Legacy | IntegrityStatus::Missing
            )
        })
        .count();
    if policy.strict && unproven > 0 {
        eprintln!(
            "warning: --strict: {unproven} {} without a note or with a note written before the integrity check; they pass for now, a later release will fail them",
            if unproven == 1 { "commit" } else { "commits" }
        );
    }
    if let Some(reason) = &record_check.reason
        && !no_record
        && !results.is_empty()
    {
        eprintln!("Server record not checked: {reason}.");
    }

    let summary = if failed > 0 {
        Some(failure_summary(failed, results.len()))
    } else if record_missing {
        Some(format!(
            "--require-record: the server record was not checked ({})",
            record_check.reason.as_deref().unwrap_or("unknown reason")
        ))
    } else {
        None
    };

    if arg_parser::json() {
        let report = VerifyReport {
            range: &target,
            commits: results.len(),
            counts,
            failed,
            results: &results,
            record_check: &record_check,
        };
        if let Some(summary) = summary {
            output::fail_with(ErrorKind::IntegrityFailed, summary, Some(&report));
        }
        output::print_json(&report);
        return;
    }

    print_human(&target, &results, policy, &record_check);
    if let Some(summary) = summary {
        output::fail_reported(ErrorKind::IntegrityFailed, summary);
    }
}

/// Whether `status` fails `autter verify`: every failure; under `--strict`
/// a note rebound by an older autter, whose digest cannot be checked (anyone
/// can make a note look rebound); under `--require-record` a note the
/// server has no record of.
fn fails(status: IntegrityStatus, policy: Policy) -> bool {
    status.is_failure()
        || (policy.strict && status == IntegrityStatus::ReboundByOlderAutter)
        || (policy.require_record && status == IntegrityStatus::NotRecorded)
}

fn failure_summary(failed: usize, total: usize) -> String {
    format!(
        "{failed} of {total} {} failed the integrity check",
        if total == 1 { "commit" } else { "commits" }
    )
}

/// The commits of `target`: a range (`a..b`, `a...b`, `^a b`) through
/// `git rev-list`, a single revision as that one commit.
fn resolve_commits(repo: &Repository, target: &str) -> Result<Vec<String>, AutterError> {
    let mut args = repo.global_args_for_exec();
    if target.contains("..") {
        args.push("rev-list".to_string());
        args.push(target.to_string());
    } else {
        args.push("rev-parse".to_string());
        args.push("--verify".to_string());
        args.push("--end-of-options".to_string());
        args.push(format!("{target}^{{commit}}"));
    }
    let out = exec_git(&args)?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn print_human(target: &str, results: &[VerifyResult], policy: Policy, record_check: &RecordCheck) {
    let total = results.len();
    if total == 0 {
        println!("No commits in {target}.");
        return;
    }
    for r in results {
        let status = r.integrity.status;
        let marker = if fails(status, policy) {
            "FAIL"
        } else if status == IntegrityStatus::Verified {
            "ok  "
        } else {
            "warn"
        };
        // Missing notes are routine on most histories: list them only on a
        // single commit, with --strict, or when the server recorded a note.
        if status == IntegrityStatus::Missing && total > 1 && !policy.strict && r.record.is_none() {
            continue;
        }
        println!(
            "{marker} {} {:<23} {}",
            &r.integrity.commit[..r.integrity.commit.len().min(8)],
            status.as_str(),
            r.integrity.detail
        );
    }
    let count = |s: IntegrityStatus| results.iter().filter(|r| r.integrity.status == s).count();
    let failed = results
        .iter()
        .filter(|r| fails(r.integrity.status, policy))
        .count();
    println!();
    println!(
        "{} {} checked: {} verified, {} legacy, {} without a note, {} failed",
        total,
        if total == 1 { "commit" } else { "commits" },
        count(IntegrityStatus::Verified),
        count(IntegrityStatus::Legacy),
        count(IntegrityStatus::Missing),
        failed
    );
    if record_check.checked {
        let record = |value: &str| {
            results
                .iter()
                .filter(|r| r.record.as_ref().is_some_and(|c| c.record == value))
                .count()
        };
        println!(
            "Server record: {} {}, {} changed after recorded, {} not recorded yet, {} recorded but missing here",
            record("matches"),
            if record("matches") == 1 {
                "matches"
            } else {
                "match"
            },
            record("changed"),
            record("not_recorded"),
            record("recorded")
        );
    }
    if failed > 0 {
        eprintln!(
            "A failed note does not belong to its commit, was changed after autter wrote it, or was carried by a rewrite from a note that failed; stats and blame count its lines as unknown."
        );
        if count(IntegrityStatus::ChangedAfterRecorded) > 0 {
            eprintln!(
                "changed_after_recorded: the server keeps every version of a note it receives; this note differs from the first version it recorded, or the record has several."
            );
        }
        if policy.strict && count(IntegrityStatus::ReboundByOlderAutter) > 0 {
            eprintln!(
                "--strict: a note rebound by an older autter cannot be checked against its digest, so it fails; run a newer autter over the range (rebase or `autter ci`) to stamp it again."
            );
        }
        if policy.require_record && count(IntegrityStatus::NotRecorded) > 0 {
            eprintln!(
                "--require-record: a note the server has no record of fails; notes upload in the background, so run `autter verify` again once the upload has happened."
            );
        }
    }
}
