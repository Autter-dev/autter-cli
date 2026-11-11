//! `-h`/`--help` must only ever print help. Users reported that
//! `autter uninstall --help` ran a full uninstall (hooks removed from every
//! agent, trace2 config cleared, service stopped), that `install --help` ran a
//! real install, and that `stats --help` failed when the background service was
//! unreachable. These tests run every subcommand with `--help` and check that
//! it exits 0 and leaves the disk exactly as it was, and that the commands that
//! change the machine reject unknown flags and won't uninstall unconfirmed.

use crate::repos::test_repo::TestRepo;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

/// Every `autter` subcommand the dispatcher knows (aliases included), plus
/// nested forms whose first word used to run before help was checked.
const HELP_INVOCATIONS: &[&[&str]] = &[
    &["checkpoint", "--help"],
    &["log", "--help"],
    &["blame", "--help"],
    &["diff", "--help"],
    &["stats", "--help"],
    &["badge", "--help"],
    &["wrapped", "--help"],
    &["file-changes", "--help"],
    &["status", "--help"],
    &["show", "--help"],
    &["show-prompt", "--help"],
    &["config", "--help"],
    &["config", "set", "prompt_storage", "local", "--help"],
    &["debug", "--help"],
    &["doctor", "--help"],
    &["sync", "--help"],
    &["sync", "purge", "--help"],
    &["sync", "open", "--help"],
    &["dashboard", "--help"],
    &["dash", "--help"],
    &["bg", "--help"],
    &["bg", "stop", "--help"],
    &["d", "start", "--help"],
    &["daemon", "restart", "-h"],
    &["install-hooks", "--help"],
    &["install", "--help"],
    &["install", "--system", "-h"],
    &["uninstall-hooks", "--help"],
    &["uninstall", "--help"],
    &["uninstall", "--yes", "--help"],
    &["uninstall", "-h"],
    &["ci", "--help"],
    &["squash-authorship", "--help"],
    &["squash-authorship", "main", "--help"],
    &["git-path", "--help"],
    &["upgrade", "--help"],
    &["fetch-notes", "--help"],
    &["onboard", "--help"],
    &["onboard", "--connect", "--help"],
    &["login", "--help"],
    &["logout", "--help"],
    &["whoami", "--help"],
    &["ask", "--help"],
    &["logs", "--help"],
    &["threads", "--help"],
    &["memory", "--help"],
    &["context", "--help"],
    &["mcp", "--help"],
    &["telemetry", "--help"],
    &["telemetry", "off", "--help"],
    &["notes", "--help"],
    &["notes", "migrate", "--help"],
    &["notes", "backfill", "--help"],
    &["memory-hook", "--help"],
    &["flush-metrics-db", "--help"],
    &["exchange-nonce", "--help"],
    &["git-hooks", "--help"],
    &["version", "--help"],
];

/// Files that change on their own while a test runs (background service logs,
/// databases, sockets, locks) and say nothing about what a command did.
fn is_runtime_state(path: &Path) -> bool {
    let text = path.to_string_lossy();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    path.components().any(|c| {
        let c = c.as_os_str().to_string_lossy();
        c == "logs" || c == "objects" || c.contains("daemon")
    }) || [
        ".log", ".sock", ".pid", ".lock", "-wal", "-shm", ".db", ".sqlite", ".sqlite3",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
        || text.contains("internal")
        || name == "index"
        || name == "FETCH_HEAD"
}

/// Every regular file under `root` with its contents, skipping runtime state.
/// Runtime state is judged on the path inside `root`, so a temp-dir name
/// higher up can never hide the whole tree.
fn snapshot(root: &Path, into: &mut BTreeMap<PathBuf, Vec<u8>>) {
    snapshot_dir(root, root, into);
}

fn snapshot_dir(root: &Path, dir: &Path, into: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if is_runtime_state(relative) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            snapshot_dir(root, &path, into);
        } else if file_type.is_file()
            && let Ok(bytes) = fs::read(&path)
        {
            into.insert(path, bytes);
        }
    }
}

fn disk_state(repo: &TestRepo) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut state = BTreeMap::new();
    snapshot(repo.test_home_path(), &mut state);
    snapshot(repo.path(), &mut state);
    assert!(
        state.contains_key(&repo.test_home_path().join(".gitconfig"))
            && state.contains_key(&repo.path().join("README.md")),
        "the disk snapshot must cover the test HOME and the repo"
    );
    state
}

fn changed_paths(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
) -> Vec<String> {
    let mut changed = Vec::new();
    for (path, bytes) in before {
        match after.get(path) {
            None => changed.push(format!("removed {}", path.display())),
            Some(now) if now != bytes => changed.push(format!("modified {}", path.display())),
            _ => {}
        }
    }
    for path in after.keys() {
        if !before.contains_key(path) {
            changed.push(format!("created {}", path.display()));
        }
    }
    changed
}

/// Run autter without a terminal on stdin, the way scripts and CI do.
fn run(repo: &TestRepo, args: &[&str]) -> Output {
    repo.autter_command(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn autter")
}

/// A repo with an initial commit and the kind of global setup install leaves
/// behind (a `[trace2]` section), so an uninstall would visibly change it.
fn repo_with_setup() -> TestRepo {
    let repo = TestRepo::new();
    fs::write(repo.path().join("README.md"), "hello\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let gitconfig = repo.test_home_path().join(".gitconfig");
    let mut contents = fs::read_to_string(&gitconfig).unwrap_or_default();
    contents.push_str("[trace2]\n\teventNesting = 5\n");
    fs::write(&gitconfig, contents).unwrap();
    repo
}

#[test]
fn every_subcommand_help_prints_help_and_changes_nothing() {
    let repo = repo_with_setup();
    let before = disk_state(&repo);
    let gitconfig_before = fs::read_to_string(repo.test_home_path().join(".gitconfig")).unwrap();

    for args in HELP_INVOCATIONS {
        let output = run(&repo, args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "`autter {}` should print help and exit 0, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
            args.join(" "),
            output.status.code(),
        );
        assert!(
            !stderr.contains("Removed") && !stdout.contains("Hooks removed"),
            "`autter {}` removed something:\nstdout: {stdout}\nstderr: {stderr}",
            args.join(" "),
        );
        assert!(
            !stderr.contains("failed to connect to autter background service"),
            "`autter {}` must not need the background service:\n{stderr}",
            args.join(" "),
        );

        let after = disk_state(&repo);
        let changed = changed_paths(&before, &after);
        assert!(
            changed.is_empty(),
            "`autter {}` changed files on disk:\n{}",
            args.join(" "),
            changed.join("\n"),
        );
    }

    // The trace2 section `uninstall` would clear is still there.
    let gitconfig_after = fs::read_to_string(repo.test_home_path().join(".gitconfig")).unwrap();
    assert_eq!(gitconfig_before, gitconfig_after);
    assert!(gitconfig_after.contains("[trace2]"));
}

#[test]
fn uninstall_help_prints_uninstall_help() {
    let repo = repo_with_setup();
    let output = run(&repo, &["uninstall", "--help"]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("autter uninstall [--yes]"),
        "expected uninstall's own help, got:\n{stderr}"
    );
}

#[test]
fn uninstall_without_terminal_or_yes_refuses_and_changes_nothing() {
    let repo = repo_with_setup();
    let before = disk_state(&repo);

    let output = run(&repo, &["uninstall"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "uninstall with no terminal and no --yes must refuse:\n{stderr}"
    );
    assert!(
        stderr.contains("--yes"),
        "refusal should name --yes:\n{stderr}"
    );
    assert!(
        changed_paths(&before, &disk_state(&repo)).is_empty(),
        "a refused uninstall must not change anything"
    );
    assert!(
        fs::read_to_string(repo.test_home_path().join(".gitconfig"))
            .unwrap()
            .contains("[trace2]")
    );
}

/// Run `autter uninstall` against an empty background-service home, so it
/// can't stop the daemon the rest of the test suite shares.
fn run_uninstall_isolated(repo: &TestRepo, args: &[&str]) -> Output {
    let lonely_home = repo.test_home_path().join("uninstall-test-service-home");
    fs::create_dir_all(&lonely_home).unwrap();
    repo.autter_command(args)
        .env("AUTTER_DAEMON_HOME", &lonely_home)
        .env_remove("AUTTER_DAEMON_CONTROL_SOCKET")
        .env_remove("AUTTER_DAEMON_TRACE_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn autter")
}

#[test]
fn uninstall_yes_removes_trace2_and_says_so() {
    let repo = repo_with_setup();
    let output = run_uninstall_isolated(&repo, &["uninstall", "--yes"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "uninstall --yes failed:\n{stderr}");
    assert!(
        !fs::read_to_string(repo.test_home_path().join(".gitconfig"))
            .unwrap()
            .contains("[trace2]"),
        "uninstall --yes should clear the trace2 section"
    );
    assert!(
        stderr.contains("global git trace2 config"),
        "uninstall should report removing trace2:\n{stderr}"
    );

    // Run again: nothing is left, so nothing may be reported as removed.
    let again = run_uninstall_isolated(&repo, &["uninstall", "--yes"]);
    let again_stderr = String::from_utf8_lossy(&again.stderr);
    assert!(
        again.status.success(),
        "second uninstall failed:\n{again_stderr}"
    );
    assert!(
        !again_stderr.contains("global git trace2 config"),
        "a second uninstall must not claim to remove trace2 again:\n{again_stderr}"
    );
}

#[test]
fn uninstall_keeps_the_users_own_trace2_settings() {
    // Someone who already traced git for their own reasons: a normal-format
    // target, a perf target and a variable of their own in envVars, next to
    // what `autter install` wrote. Uninstall used to delete the whole
    // `[trace2]` section.
    let repo = TestRepo::new();
    fs::write(repo.path().join("README.md"), "hello\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let gitconfig = repo.test_home_path().join(".gitconfig");
    let mut contents = fs::read_to_string(&gitconfig).unwrap_or_default();
    contents.push_str(
        "[trace2]\n\
         \tnormalTarget = /tmp/my-git-trace.log\n\
         \tperfTarget = /tmp/my-git-perf.log\n\
         \teventTarget = af_unix:stream:/tmp/autter-d-0000/trace.sock\n\
         \teventNesting = 10\n\
         \tenvVars = MY_VAR,AUTTER_WRAPPER_INVOCATION_ID,CLAUDE_CODE_REMOTE,HOSTNAME,CURSOR_AGENT,CODEX_INTERNAL_ORIGINATOR_OVERRIDE,CODEX_THREAD_ID,AUTTER_CLOUD_AGENT\n",
    );
    fs::write(&gitconfig, contents).unwrap();

    let dry = run_uninstall_isolated(&repo, &["uninstall", "--dry-run"]);
    let dry_stderr = String::from_utf8_lossy(&dry.stderr);
    assert!(
        dry_stderr.contains("your own trace2 settings stay"),
        "{dry_stderr}"
    );

    let output = run_uninstall_isolated(&repo, &["uninstall", "--yes"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "uninstall --yes failed:\n{stderr}");
    assert!(
        stderr.contains("global git trace2 config"),
        "uninstall should report removing its trace2 settings:\n{stderr}"
    );

    let after = fs::read_to_string(&gitconfig).unwrap();
    for kept in [
        "normalTarget = /tmp/my-git-trace.log",
        "perfTarget = /tmp/my-git-perf.log",
        "envVars = MY_VAR\n",
    ] {
        assert!(after.contains(kept), "`{kept}` must be kept:\n{after}");
    }
    for gone in [
        "eventTarget",
        "eventNesting",
        "CURSOR_AGENT",
        "autter-d-0000",
    ] {
        assert!(!after.contains(gone), "`{gone}` must be removed:\n{after}");
    }

    // Nothing of autter's is left, so a second run reports nothing removed.
    let again = run_uninstall_isolated(&repo, &["uninstall", "--yes"]);
    let again_stderr = String::from_utf8_lossy(&again.stderr);
    assert!(
        !again_stderr.contains("global git trace2 config"),
        "{again_stderr}"
    );
    assert_eq!(fs::read_to_string(&gitconfig).unwrap(), after);
}

#[test]
fn state_changing_commands_reject_unknown_flags() {
    let repo = repo_with_setup();
    let before = disk_state(&repo);

    for args in [
        &["install-hooks", "--sytem"][..],
        &["install", "--bogus"][..],
        &["uninstall-hooks", "--force"][..],
        &["uninstall", "--all"][..],
        &["uninstall", "--yes", "--everything"][..],
        &["squash-authorship", "--force", "a", "b"][..],
        &["show", "--bogus"][..],
        &["logout", "--bogus"][..],
        &["dashboard", "--bogus"][..],
    ] {
        let output = run(&repo, args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`autter {}` should be a usage error:\n{stderr}",
            args.join(" ")
        );
        assert!(
            stderr.contains("unknown option"),
            "`autter {}` should name the unknown option:\n{stderr}",
            args.join(" ")
        );
    }

    assert!(
        changed_paths(&before, &disk_state(&repo)).is_empty(),
        "rejected commands must not change anything"
    );
}
