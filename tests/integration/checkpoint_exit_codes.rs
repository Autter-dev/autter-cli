//! `autter checkpoint` used to exit 0 even when the checkpoint could never
//! become attribution (git not reporting commits, not in a repository, the
//! background service unreachable), so scripts and people running it by hand
//! couldn't tell "recorded" from "lost".
//!
//! Direct runs (the manual presets with no hook payload) now exit non-zero
//! with the fix. Hook runs (agent presets, editor extensions, or any
//! `--hook-input`) still exit 0 so an agent's edit flow never breaks.
//!
//! The harness routes trace2 per command (`GIT_TRACE2_EVENT`), so these tests
//! opt into the trace2 check with `AUTTER_TEST_ENFORCE_CAPTURE_CHECK=1` and
//! blank that variable to simulate a machine where git isn't set up.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use autter::daemon::DaemonConfig;
use std::fs;

const ENFORCE: (&str, &str) = ("AUTTER_TEST_ENFORCE_CAPTURE_CHECK", "1");
/// No per-command trace2 routing: only the global git config counts.
const NO_TRACE2_ENV: (&str, &str) = ("GIT_TRACE2_EVENT", "");

/// The trace2 global config `autter install --system` writes, pointed at this
/// test's isolated service.
fn write_trace2_global_config(repo: &TestRepo) {
    let target = DaemonConfig::trace2_event_target_for_path(&repo.daemon_trace_socket_path());
    repo.git_og(&["config", "--global", "trace2.eventTarget", &target])
        .expect("write trace2.eventTarget");
    repo.git_og(&["config", "--global", "trace2.eventNesting", "10"])
        .expect("write trace2.eventNesting");
}

#[test]
fn direct_checkpoint_fails_when_git_does_not_report_commits() {
    let repo = TestRepo::new();
    let path = repo.path().join("app.txt");
    fs::write(&path, "base line\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["base line".unattributed_human()]);

    fs::write(&path, "base line\nai line\n").unwrap();
    let err = repo
        .autter_with_env(
            &["checkpoint", "mock_ai", "app.txt"],
            &[ENFORCE, NO_TRACE2_ENV],
        )
        .expect_err("a checkpoint that won't become attribution must exit non-zero");
    assert!(
        err.contains("git isn't set up to report commits to autter"),
        "{err}"
    );
    assert!(
        err.contains("Fix: run `autter install --system`, then `autter doctor`"),
        "{err}"
    );
}

#[test]
fn hook_checkpoint_stays_non_blocking_when_git_does_not_report_commits() {
    let repo = TestRepo::new();
    let path = repo.path().join("app.txt");
    fs::write(&path, "base line\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["base line".unattributed_human()]);

    fs::write(&path, "base line\nai line\n").unwrap();
    // The same checkpoint, but carrying a hook payload as agent hooks do: it
    // must never fail the agent's edit, whatever the machine's setup.
    let payload = serde_json::json!({
        "file_paths": [path.to_string_lossy()],
        "cwd": repo.path().to_string_lossy(),
    })
    .to_string();
    repo.autter_with_env(
        &["checkpoint", "mock_ai", "--hook-input", &payload],
        &[ENFORCE, NO_TRACE2_ENV],
    )
    .expect("hook checkpoints exit 0");

    repo.stage_all_and_commit("ai edit").unwrap();
    file.assert_committed_lines(lines!["base line".unattributed_human(), "ai line".ai()]);
}

#[test]
fn direct_checkpoint_succeeds_when_capture_is_set_up() {
    let repo = TestRepo::new();
    write_trace2_global_config(&repo);
    let path = repo.path().join("app.txt");
    fs::write(&path, "base line\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["base line".unattributed_human()]);

    fs::write(&path, "base line\nai line\n").unwrap();
    repo.autter_with_env(
        &["checkpoint", "mock_ai", "app.txt"],
        &[ENFORCE, NO_TRACE2_ENV],
    )
    .expect("with git capture configured, a direct checkpoint exits 0");

    repo.stage_all_and_commit("ai edit").unwrap();
    file.assert_committed_lines(lines!["base line".unattributed_human(), "ai line".ai()]);
}

#[test]
fn direct_checkpoint_outside_a_repository_fails_but_a_hook_does_not() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("app.txt"), "base line\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["base line".unattributed_human()]);

    let outside = repo.test_home_path().join("notes.txt");
    fs::write(&outside, "not in any repository\n").unwrap();
    let outside = outside.to_string_lossy().to_string();

    let err = repo
        .autter(&["checkpoint", "mock_ai", &outside])
        .expect_err("a direct checkpoint of a file outside any repository must exit non-zero");
    assert!(err.contains("No git repository found"), "{err}");
    assert!(err.contains("Fix: run it inside a git repository"), "{err}");

    let payload = serde_json::json!({
        "file_paths": [outside],
        "cwd": repo.test_home_path().to_string_lossy(),
    })
    .to_string();
    let output = repo
        .autter(&["checkpoint", "mock_ai", "--hook-input", &payload])
        .expect("hook checkpoints exit 0 even outside a repository");
    assert!(output.contains("preset error"), "{output}");
}
