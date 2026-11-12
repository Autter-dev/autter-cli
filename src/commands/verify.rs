//! `autter verify [<rev|range>] [--json] [--strict]`: the integrity check of
//! authorship notes (see `authorship::note_integrity`).
//!
//! Exit codes: 0 when no note fails; [`EXIT_INTEGRITY_FAILED`] (5) when a note
//! belongs to another commit, was changed, or was carried by a rewrite from a
//! note that failed. Missing and legacy notes are
//! warnings, also with `--strict` for now (a later release will fail them
//! under `--strict`).

use crate::authorship::note_integrity::{self, CommitIntegrity, IntegrityStatus};
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
    results: &'a [CommitIntegrity],
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
    for arg in &pp.rest {
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
    let results = match note_integrity::verify_commits(&repo, &commits) {
        Ok(results) => results,
        Err(e) => output::fail(ErrorKind::Runtime, format!("autter verify: {e}")),
    };

    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for r in &results {
        *counts.entry(r.status.as_str()).or_default() += 1;
    }
    let failed = results.iter().filter(|r| r.status.is_failure()).count();
    let strict = output::strict();
    let unproven = results
        .iter()
        .filter(|r| matches!(r.status, IntegrityStatus::Legacy | IntegrityStatus::Missing))
        .count();
    if strict && unproven > 0 {
        eprintln!(
            "warning: --strict: {unproven} {} without a note or with a note written before the integrity check; they pass for now, a later release will fail them",
            if unproven == 1 { "commit" } else { "commits" }
        );
    }

    if arg_parser::json() {
        let report = VerifyReport {
            range: &target,
            commits: results.len(),
            counts,
            failed,
            results: &results,
        };
        if failed > 0 {
            output::fail_with(
                ErrorKind::IntegrityFailed,
                failure_summary(failed, results.len()),
                Some(&report),
            );
        }
        output::print_json(&report);
        return;
    }

    print_human(&target, &results, strict);
    if failed > 0 {
        output::fail_reported(
            ErrorKind::IntegrityFailed,
            failure_summary(failed, results.len()),
        );
    }
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

fn print_human(target: &str, results: &[CommitIntegrity], strict: bool) {
    let total = results.len();
    if total == 0 {
        println!("No commits in {target}.");
        return;
    }
    for r in results {
        let marker = if r.status.is_failure() {
            "FAIL"
        } else if r.status == IntegrityStatus::Verified {
            "ok  "
        } else {
            "warn"
        };
        // Missing notes are routine on most histories: list them only on a
        // single commit or with --strict.
        if r.status == IntegrityStatus::Missing && total > 1 && !strict {
            continue;
        }
        println!(
            "{marker} {} {:<23} {}",
            &r.commit[..r.commit.len().min(8)],
            r.status.as_str(),
            r.detail
        );
    }
    let count = |s: IntegrityStatus| results.iter().filter(|r| r.status == s).count();
    let failed = results.iter().filter(|r| r.status.is_failure()).count();
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
    if failed > 0 {
        eprintln!(
            "A failed note does not belong to its commit, was changed after autter wrote it, or was carried by a rewrite from a note that failed; stats and blame count its lines as unknown."
        );
    }
}
