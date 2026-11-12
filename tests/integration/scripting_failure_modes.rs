//! docs/scripting.md, failure side: every documented `--json` command
//! (`stats`, `status`, `log`, `show`, `blame`, `diff`, `whoami`, `doctor`,
//! `sync status`) in every way it can fail before producing a report: a bad
//! flag, a revision that does not resolve, outside a repository, and with the
//! background service unreachable. Each run must print exactly one JSON
//! document on stdout, with the documented `error.code`, and exit with the
//! documented code.

use crate::json_contract::{Run, assert_valid, parse_document, repo_with_ai_commit, run_output};
use crate::repos::test_repo::TestRepo;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// An option the command does not have, or a subcommand that does not
    /// exist.
    BadFlag,
    /// A commit, revision or range that does not resolve.
    BadRevision,
    /// Run in a directory that is not inside a git repository.
    OutsideRepository,
    /// The background service cannot be reached.
    ServiceUnreachable,
}

#[derive(Clone, Copy, Debug)]
enum Expect {
    /// `{schema_version, error}` with this `error.code`, and this exit code.
    Error(&'static str, i32),
    /// The failure mode does not apply to this command (it does not read a
    /// repository, or does not need the service), so it runs: one document
    /// of its own schema, and an exit code that matches `error.exit_code`
    /// when there is an error.
    Runs(&'static str),
}

const USAGE: Expect = Expect::Error("usage_error", 2);
const REVISION: Expect = Expect::Error("revision_not_found", 1);
const NOT_A_REPOSITORY: Expect = Expect::Error("not_a_repository", 2);
const UNREACHABLE: Expect = Expect::Error("service_unreachable", 1);

/// (failure mode, args, expected). `doctor` rows run against their own
/// background service (see `run_case`).
const CASES: &[(Mode, &[&str], Expect)] = &[
    // --- a bad flag -------------------------------------------------------
    (Mode::BadFlag, &["stats", "--bogus", "--json"], USAGE),
    (Mode::BadFlag, &["status", "--bogus", "--json"], USAGE),
    (Mode::BadFlag, &["log", "--bogus", "--json"], USAGE),
    (Mode::BadFlag, &["log", "-n", "many", "--json"], USAGE),
    (Mode::BadFlag, &["log", "--json", "--graph"], USAGE),
    (Mode::BadFlag, &["show", "--bogus", "--json"], USAGE),
    (
        Mode::BadFlag,
        &["blame", "app.txt", "--bogus", "--json"],
        USAGE,
    ),
    (Mode::BadFlag, &["diff", "HEAD", "--bogus", "--json"], USAGE),
    (Mode::BadFlag, &["whoami", "--bogus", "--json"], USAGE),
    (Mode::BadFlag, &["doctor", "--bogus", "--json"], USAGE),
    (
        Mode::BadFlag,
        &["sync", "status", "--bogus", "--json"],
        USAGE,
    ),
    // The dispatcher's own failures: an unknown subcommand, an unknown
    // command, and a leading flag it cannot parse.
    (Mode::BadFlag, &["sync", "bogus", "--json"], USAGE),
    (Mode::BadFlag, &["--json", "sync", "bogus"], USAGE),
    (Mode::BadFlag, &["bogus", "--json"], USAGE),
    (Mode::BadFlag, &["--json", "-C"], USAGE),
    (
        Mode::BadFlag,
        &["-C", "/nonexistent-autter-dir", "stats", "--json"],
        Expect::Error("runtime_error", 1),
    ),
    // --- a revision that does not resolve ---------------------------------
    (
        Mode::BadRevision,
        &["stats", "nosuchrev", "--json"],
        REVISION,
    ),
    (
        Mode::BadRevision,
        &["stats", "nosuchrev..HEAD", "--json"],
        REVISION,
    ),
    (Mode::BadRevision, &["log", "nosuchrev", "--json"], REVISION),
    (
        Mode::BadRevision,
        &["log", "nosuchrev..HEAD", "--json"],
        REVISION,
    ),
    (
        Mode::BadRevision,
        &["show", "nosuchrev", "--json"],
        REVISION,
    ),
    (
        Mode::BadRevision,
        &["show", "nosuchrev..HEAD", "--json"],
        REVISION,
    ),
    (
        Mode::BadRevision,
        &["blame", "app.txt", "--ignore-rev", "nosuchrev", "--json"],
        REVISION,
    ),
    (
        Mode::BadRevision,
        &["diff", "nosuchrev", "--json"],
        REVISION,
    ),
    (
        Mode::BadRevision,
        &["diff", "nosuchrev..HEAD", "--json"],
        REVISION,
    ),
    // `status` takes no revision: a stray argument is a usage error.
    (Mode::BadRevision, &["status", "nosuchrev", "--json"], USAGE),
    // --- outside a repository ---------------------------------------------
    (
        Mode::OutsideRepository,
        &["stats", "--json"],
        NOT_A_REPOSITORY,
    ),
    (
        Mode::OutsideRepository,
        &["status", "--json"],
        NOT_A_REPOSITORY,
    ),
    (
        Mode::OutsideRepository,
        &["log", "--json"],
        NOT_A_REPOSITORY,
    ),
    (
        Mode::OutsideRepository,
        &["show", "HEAD", "--json"],
        NOT_A_REPOSITORY,
    ),
    (
        Mode::OutsideRepository,
        &["blame", "app.txt", "--json"],
        NOT_A_REPOSITORY,
    ),
    (
        Mode::OutsideRepository,
        &["diff", "HEAD", "--json"],
        NOT_A_REPOSITORY,
    ),
    // These three do not read a repository.
    (
        Mode::OutsideRepository,
        &["whoami", "--json"],
        Expect::Runs("whoami"),
    ),
    (
        Mode::OutsideRepository,
        &["sync", "status", "--json"],
        Expect::Runs("sync_status"),
    ),
    (
        Mode::OutsideRepository,
        &["doctor", "--json"],
        Expect::Runs("doctor"),
    ),
    // --- the background service is unreachable ----------------------------
    (Mode::ServiceUnreachable, &["stats", "--json"], UNREACHABLE),
    (Mode::ServiceUnreachable, &["--json", "stats"], UNREACHABLE),
    (Mode::ServiceUnreachable, &["status", "--json"], UNREACHABLE),
    (Mode::ServiceUnreachable, &["log", "--json"], UNREACHABLE),
    (
        Mode::ServiceUnreachable,
        &["show", "HEAD", "--json"],
        UNREACHABLE,
    ),
    (
        Mode::ServiceUnreachable,
        &["blame", "app.txt", "--json"],
        UNREACHABLE,
    ),
    (
        Mode::ServiceUnreachable,
        &["diff", "HEAD", "--json"],
        UNREACHABLE,
    ),
    // A command that does not exist is a usage error whether or not the
    // service is up; `--json` after it still gets its one document.
    (Mode::ServiceUnreachable, &["bogus", "--json"], USAGE),
    (Mode::ServiceUnreachable, &["--json", "bogus"], USAGE),
    // These three work without the service, so that a broken setup can
    // still be diagnosed.
    (
        Mode::ServiceUnreachable,
        &["whoami", "--json"],
        Expect::Runs("whoami"),
    ),
    (
        Mode::ServiceUnreachable,
        &["sync", "status", "--json"],
        Expect::Runs("sync_status"),
    ),
    (
        Mode::ServiceUnreachable,
        &["doctor", "--json"],
        Expect::Runs("doctor"),
    ),
];

fn run_case(repo: &TestRepo, outside: &std::path::Path, mode: Mode, args: &[&str]) -> Run {
    let mut command = repo.autter_command(args);
    command.env_remove("AUTTER_STRICT");
    match mode {
        Mode::OutsideRepository => {
            command.current_dir(outside);
            command.env("GIT_CEILING_DIRECTORIES", outside);
        }
        Mode::ServiceUnreachable => {
            command.env("AUTTER_TEST_DAEMON_UNAVAILABLE", "1");
        }
        Mode::BadFlag | Mode::BadRevision => {}
    }
    run_output(command.output().expect("run autter"))
}

fn check_case(run: &Run, mode: Mode, args: &[&str], expect: Expect) -> Result<(), String> {
    let what = format!("{mode:?}: autter {}", args.join(" "));
    let describe = |problem: String| {
        format!(
            "{what}: {problem}\n  exit: {}\n  stdout: {}\n  stderr: {}",
            run.code,
            run.stdout.trim(),
            run.stderr.trim().lines().next().unwrap_or("")
        )
    };
    // Exactly one JSON document on stdout, and nothing else.
    let document: serde_json::Value = serde_json::from_str(run.stdout.trim())
        .map_err(|e| describe(format!("stdout is not exactly one JSON document ({e})")))?;
    match expect {
        Expect::Error(code, exit) => {
            if run.code != exit {
                return Err(describe(format!("expected exit {exit}")));
            }
            if document["error"]["code"] != code {
                return Err(describe(format!("expected error.code {code}")));
            }
            if document["error"]["exit_code"] != exit {
                return Err(describe(format!("expected error.exit_code {exit}")));
            }
            if document["error"]["message"]
                .as_str()
                .is_none_or(str::is_empty)
            {
                return Err(describe("error.message is empty".to_string()));
            }
            assert_valid(&document, "error_only", &what);
            // The same message is on stderr for a person reading the CI log.
            if run.stderr.trim().is_empty() {
                return Err(describe("nothing on stderr".to_string()));
            }
        }
        Expect::Runs(def) => {
            assert_valid(&parse_document(run, &what), def, &what);
            match document.get("error") {
                Some(error) => {
                    if error["exit_code"] != run.code || run.code == 0 {
                        return Err(describe(
                            "error.exit_code does not match the exit code".to_string(),
                        ));
                    }
                }
                None => {
                    if run.code != 0 {
                        return Err(describe(
                            "non-zero exit without an error object".to_string(),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn run_table(repo: &TestRepo, doctor: bool) {
    let outside = tempfile::tempdir().unwrap();
    let mut problems = Vec::new();
    let mut checked = 0;
    for (mode, args, expect) in CASES {
        if args.contains(&"doctor") != doctor {
            continue;
        }
        let run = run_case(repo, outside.path(), *mode, args);
        if let Err(problem) = check_case(&run, *mode, args, *expect) {
            problems.push(problem);
        }
        checked += 1;
    }
    assert!(checked > 0);
    assert!(
        problems.is_empty(),
        "{} of {checked} cases do not honour docs/scripting.md:\n\n{}",
        problems.len(),
        problems.join("\n\n")
    );
}

#[test]
fn every_json_command_fails_with_one_document_and_the_documented_code() {
    let repo = repo_with_ai_commit();
    repo.sync_daemon_force();
    run_table(&repo, false);
}

/// Doctor's end-to-end checks drive the background service; keep them off the
/// one other tests share.
#[test]
fn doctor_json_fails_with_one_document_and_the_documented_code() {
    let repo = TestRepo::new_dedicated_daemon();
    run_table(&repo, true);
}

/// Every command, failure mode and documented code is in the table.
#[test]
fn the_failure_table_covers_every_documented_command_and_mode() {
    for command in [
        "stats", "status", "log", "show", "blame", "diff", "whoami", "doctor", "sync",
    ] {
        for mode in [
            Mode::BadFlag,
            Mode::OutsideRepository,
            Mode::ServiceUnreachable,
        ] {
            assert!(
                CASES.iter().any(|(case_mode, args, _)| *case_mode == mode
                    && args.iter().find(|arg| !arg.starts_with('-')) == Some(&command)),
                "no {mode:?} case for `autter {command}`"
            );
        }
    }
    // The commands that take a revision.
    for command in ["stats", "log", "show", "blame", "diff"] {
        assert!(
            CASES
                .iter()
                .any(|(mode, args, _)| *mode == Mode::BadRevision && args[0] == command),
            "no bad-revision case for `autter {command}`"
        );
    }
}

/// The exit code does not depend on `--json`: the same failures without it
/// exit the same way and print nothing on stdout.
#[test]
fn failures_exit_the_same_way_without_json() {
    let repo = repo_with_ai_commit();
    repo.sync_daemon_force();
    let outside = tempfile::tempdir().unwrap();
    for (mode, args, expect) in CASES {
        let Expect::Error(_, exit) = expect else {
            continue;
        };
        if args.contains(&"doctor") {
            continue;
        }
        let plain: Vec<&str> = args
            .iter()
            .copied()
            .filter(|arg| *arg != "--json")
            .collect();
        let run = run_case(&repo, outside.path(), *mode, &plain);
        assert_eq!(
            run.code,
            *exit,
            "{mode:?}: autter {}: stderr: {}",
            plain.join(" "),
            run.stderr
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(run.stdout.trim()).is_err(),
            "autter {}: no JSON without --json: {}",
            plain.join(" "),
            run.stdout
        );
    }
}
