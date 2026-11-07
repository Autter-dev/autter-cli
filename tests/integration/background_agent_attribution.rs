use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{NewCommit, TestRepo};
use std::fs;

/// Environment of a git command run inside a no-hooks background agent.
const AGENT_ENV: &[(&str, &str)] = &[("AUTTER_CLOUD_AGENT", "1")];

/// Stage everything and commit from a shell that runs inside a background agent.
fn agent_commit(repo: &TestRepo, message: &str) -> NewCommit {
    repo.stage_all_and_commit_with_env(message, AGENT_ENV)
        .unwrap()
}

/// When a commit is made inside a no-hooks background agent (simulated via
/// `AUTTER_CLOUD_AGENT=1` on the commit), it should be attributed wholly
/// to the detected AI tool even though no checkpoints were fired.
#[test]
fn test_no_hooks_agent_all_lines_attributed() {
    let repo = TestRepo::new();

    fs::write(repo.path().join("file.txt"), "alpha\nbeta\ngamma\n").unwrap();
    agent_commit(&repo, "first commit");

    let mut file = repo.filename("file.txt");
    file.assert_committed_lines(crate::lines!["alpha".ai(), "beta".ai(), "gamma".ai()]);
}

/// Multiple files in a single commit are all attributed.
#[test]
fn test_no_hooks_agent_multiple_files() {
    let repo = TestRepo::new();

    fs::write(repo.path().join("a.txt"), "line a\n").unwrap();
    fs::write(repo.path().join("b.txt"), "line b\n").unwrap();
    agent_commit(&repo, "multi file");

    let mut a = repo.filename("a.txt");
    a.assert_committed_lines(crate::lines!["line a".ai()]);
    let mut b = repo.filename("b.txt");
    b.assert_committed_lines(crate::lines!["line b".ai()]);
}

/// When lines already have explicit attribution (e.g. from a KnownHuman
/// checkpoint fired by an IDE extension), only the unattributed "holes"
/// get the background agent attribution.
#[test]
fn test_no_hooks_agent_preserves_existing_attribution() {
    let repo = TestRepo::new();

    // Seed commit so we have a base
    fs::write(repo.path().join("seed.txt"), "seed\n").unwrap();
    agent_commit(&repo, "seed");
    repo.filename("seed.txt")
        .assert_committed_lines(crate::lines!["seed".ai()]);

    // Simulate: human types some lines (KnownHuman checkpoint), then agent adds more
    // without firing its own checkpoint.
    let content = "human typed this\nagent added this\nagent also added this\n";
    fs::write(repo.path().join("mixed.txt"), content).unwrap();
    // Fire a KnownHuman checkpoint for the file BEFORE the agent edits
    // (simulates IDE extension detecting human keystrokes for just the first line)
    fs::write(repo.path().join("mixed.txt"), "human typed this\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "mixed.txt"])
        .unwrap();

    // Now the agent adds more lines without firing a checkpoint
    fs::write(
        repo.path().join("mixed.txt"),
        "human typed this\nagent added this\nagent also added this\n",
    )
    .unwrap();
    agent_commit(&repo, "mixed edit");

    let mut file = repo.filename("mixed.txt");
    file.assert_committed_lines(crate::lines![
        "human typed this".human(),
        "agent added this".ai(),
        "agent also added this".ai(),
    ]);
}

/// When the background agent modifies an existing file (appends lines),
/// only the new lines get attributed.
#[test]
fn test_no_hooks_agent_append_to_existing_file() {
    let repo = TestRepo::new();

    fs::write(repo.path().join("file.txt"), "original\n").unwrap();
    agent_commit(&repo, "initial");

    let mut file = repo.filename("file.txt");
    file.assert_committed_lines(crate::lines!["original".ai()]);

    fs::write(repo.path().join("file.txt"), "original\nnew line\n").unwrap();
    agent_commit(&repo, "append");

    file.assert_committed_lines(crate::lines!["original".ai(), "new line".ai()]);
}

/// Negative control: same shape, no env var. Lines that arrived without any
/// checkpoint are untracked.
#[test]
fn test_without_background_agent_env_lines_are_untracked() {
    let repo = TestRepo::new();

    fs::write(repo.path().join("plain.txt"), "alpha\nbeta\n").unwrap();
    repo.stage_all_and_commit("no agent").unwrap();

    let mut file = repo.filename("plain.txt");
    file.assert_committed_lines(crate::lines![
        "alpha".unattributed_human(),
        "beta".unattributed_human(),
    ]);
}

/// With-hooks agents (Claude Code remote, Cursor) should NOT trigger
/// blanket attribution — they fire their own checkpoints.
#[test]
fn test_with_hooks_agent_does_not_blanket_attribute() {
    let repo = TestRepo::new();

    fs::write(repo.path().join("file.txt"), "line\n").unwrap();
    repo.stage_all_and_commit_with_env("commit", &[("CLAUDE_CODE_REMOTE", "true")])
        .unwrap();

    let mut file = repo.filename("file.txt");
    file.assert_committed_lines(crate::lines!["line".unattributed_human()]);
}

/// Rebase must NOT re-attribute lines to the background agent. The original
/// attribution (from the commit being rebased) should be preserved through
/// the rebase operation.
#[test]
fn test_rebase_does_not_reattribute_to_bg_agent() {
    let repo = TestRepo::new();

    // Commit 1: base
    fs::write(repo.path().join("file.txt"), "base line\n").unwrap();
    agent_commit(&repo, "base");

    // Commit 2: AI adds a line (attributed to bg agent via our new logic)
    fs::write(repo.path().join("file.txt"), "base line\nai line\n").unwrap();
    agent_commit(&repo, "ai commit");

    let mut file = repo.filename("file.txt");
    file.assert_committed_lines(crate::lines!["base line".ai(), "ai line".ai()]);

    // Create a side branch from base and add a different file
    repo.git(&["checkout", "-b", "side", "HEAD~1"]).unwrap();
    fs::write(repo.path().join("other.txt"), "side content\n").unwrap();
    agent_commit(&repo, "side commit");
    repo.filename("other.txt")
        .assert_committed_lines(crate::lines!["side content".ai()]);

    // Go back to main and rebase onto side (from the agent shell, as it would be)
    repo.git(&["checkout", "-"]).unwrap();
    repo.git_with_env(&["rebase", "side"], AGENT_ENV, None)
        .unwrap();

    // After rebase, the AI line should still be attributed to AI (the same
    // agent), not re-attributed or doubled.
    file.assert_committed_lines(crate::lines!["base line".ai(), "ai line".ai()]);
}

/// Amend preserves existing attribution and fills holes for new lines.
#[test]
fn test_amend_preserves_existing_and_attributes_new_lines() {
    let repo = TestRepo::new();

    // Commit with explicit AI checkpoint
    fs::write(repo.path().join("file.txt"), "original\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "file.txt"]).unwrap();
    agent_commit(&repo, "initial");

    let mut file = repo.filename("file.txt");
    file.assert_committed_lines(crate::lines!["original".ai()]);

    // Amend: add a line without firing a checkpoint
    fs::write(repo.path().join("file.txt"), "original\namended line\n").unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git_with_env(&["commit", "--amend", "--no-edit"], AGENT_ENV, None)
        .unwrap();

    // Original AI attribution preserved; new line attributed to bg agent
    file.assert_committed_lines(crate::lines!["original".ai(), "amended line".ai()]);
}

/// Regression: the background service keeps the environment it was started with. A
/// cloud-agent variable in that environment must not turn ordinary commits from a
/// clean shell into agent commits.
#[test]
fn test_daemon_env_does_not_mark_clean_commits_as_agent() {
    let repo = TestRepo::new_with_daemon_env(&[
        ("AUTTER_CLOUD_AGENT", "1"),
        ("CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "codex_web_agent"),
        ("CLOUD_AGENT_SESSION", "stale"),
    ]);

    fs::write(repo.path().join("file.txt"), "typed by a person\n").unwrap();
    repo.stage_all_and_commit("human commit").unwrap();

    let mut file = repo.filename("file.txt");
    file.assert_committed_lines(crate::lines!["typed by a person".unattributed_human()]);
}

/// Detection is per commit: an agent commit and a human commit in the same repo,
/// served by the same background service, are attributed independently.
#[test]
fn test_agent_detection_is_per_commit() {
    let repo = TestRepo::new();

    fs::write(repo.path().join("file.txt"), "from the agent\n").unwrap();
    agent_commit(&repo, "agent commit");
    let mut file = repo.filename("file.txt");
    file.assert_committed_lines(crate::lines!["from the agent".ai()]);

    fs::write(
        repo.path().join("file.txt"),
        "from the agent\nfrom a person\n",
    )
    .unwrap();
    repo.stage_all_and_commit("human commit").unwrap();
    file.assert_committed_lines(crate::lines![
        "from the agent".ai(),
        "from a person".unattributed_human(),
    ]);
}

/// Arbitrary `CLOUD_AGENT_*` variables are no longer treated as a background agent:
/// only the explicit, documented variables are.
#[test]
fn test_generic_cloud_agent_prefix_is_not_an_agent() {
    let repo = TestRepo::new();

    fs::write(repo.path().join("file.txt"), "line\n").unwrap();
    repo.stage_all_and_commit_with_env("commit", &[("CLOUD_AGENT_ID", "abc")])
        .unwrap();

    let mut file = repo.filename("file.txt");
    file.assert_committed_lines(crate::lines!["line".unattributed_human()]);
}
