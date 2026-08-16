//! Automatic authorship recovery after remote squash merges.
//!
//! When a PR is squash-merged on the hosting provider (GitHub/GitLab merge
//! button), the squash commit is created server-side: no local hook runs and
//! no authorship note is written for it. These tests fabricate that server-side
//! squash commit with plain git (no autter involved) in the bare upstream, then
//! verify that a plain `git pull` automatically reconstructs the authorship
//! note from the local source branch — the same recovery `autter
//! squash-authorship` performs manually.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, real_git_executable};
use rand::RngExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run plain git (never the autter proxy) in `dir`, panicking on failure.
/// The test process HOME is already isolated, so this cannot pick up the real
/// user's git config or talk to a real daemon.
fn run_real_git_in(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(real_git_executable())
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git -C {:?} {:?}: {}", dir, args, e));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "git -C {:?} {:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        dir,
        args,
        output.status.code(),
        stdout,
        stderr
    );
    stdout.trim().to_string()
}

fn scratch_clone_of(upstream: &TestRepo) -> PathBuf {
    let scratch = std::env::temp_dir().join(format!(
        "autter-squash-recovery-scratch-{}-{}",
        std::process::id(),
        rand::rng().random_range(0..u64::MAX)
    ));
    let output = Command::new(real_git_executable())
        .arg("clone")
        .arg(upstream.path())
        .arg(&scratch)
        .output()
        .expect("failed to run git clone for scratch repo");
    assert!(
        output.status.success(),
        "scratch clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    scratch
}

/// Common setup: initial commit pushed to upstream main, plus a pushed
/// `feature` branch with AI-authored content. Returns (local, upstream,
/// default_branch).
fn setup_repo_with_pushed_ai_feature_branch(
    feature_lines: Vec<crate::repos::test_file::ExpectedLine>,
) -> (TestRepo, TestRepo, String) {
    let (local, upstream) = TestRepo::new_with_remote();

    let mut base = local.filename("base.txt");
    base.set_contents(crate::lines!["base line"]);
    local.stage_all_and_commit("initial commit").unwrap();
    let default_branch = local.current_branch();
    local.git(&["push", "-u", "origin", "HEAD"]).unwrap();

    // AI feature branch, pushed like a PR branch.
    local.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = local.filename("feature.txt");
    feature.set_contents(feature_lines);
    local.stage_all_and_commit("Add AI feature").unwrap();
    local.git(&["push", "origin", "feature"]).unwrap();
    local.git(&["checkout", &default_branch]).unwrap();

    (local, upstream, default_branch)
}

/// The provider squash-merges an up-to-date PR branch: the squash commit's
/// tree equals the branch tip's tree (tree-equality detection path).
#[test]
fn test_pull_recovers_authorship_after_remote_squash_merge() {
    let (local, upstream, default_branch) =
        setup_repo_with_pushed_ai_feature_branch(crate::lines![
            "AI feature line 1".ai(),
            "AI feature line 2".ai()
        ]);

    // Fabricate the squash merge server-side in the bare upstream, exactly
    // like GitHub's merge button: a commit-tree of the branch tip's tree onto
    // main, created with plain git so no autter hook is involved.
    let feature_tree =
        run_real_git_in(upstream.path(), &["rev-parse", "refs/heads/feature^{tree}"]);
    let main_ref = format!("refs/heads/{}", default_branch);
    let main_sha = run_real_git_in(upstream.path(), &["rev-parse", &main_ref]);
    let squash_sha = run_real_git_in(
        upstream.path(),
        &[
            "commit-tree",
            &feature_tree,
            "-p",
            &main_sha,
            "-m",
            "Add AI feature (#1)",
        ],
    );
    run_real_git_in(upstream.path(), &["update-ref", &main_ref, &squash_sha]);

    local.git(&["config", "pull.rebase", "false"]).unwrap();
    local.git(&["pull"]).unwrap();

    let head = local
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(
        head, squash_sha,
        "pull should fast-forward to the squash commit"
    );

    assert!(
        local.read_authorship_note(&squash_sha).is_some(),
        "pulling a remote squash merge should automatically reconstruct its authorship note from the local feature branch"
    );

    let mut feature = local.filename("feature.txt");
    feature.assert_lines_and_blame(crate::lines![
        "AI feature line 1".ai(),
        "AI feature line 2".ai()
    ]);
}

/// Main advanced (a colleague landed an unrelated commit) before the PR was
/// squash-merged, so the squash commit's tree no longer matches the branch
/// tip's tree and detection has to fall back to patch-id equivalence. Also
/// verifies the colleague's ordinary commit is not matched to anything.
#[test]
fn test_pull_recovers_authorship_when_base_advanced_after_branch() {
    let (local, upstream, default_branch) =
        setup_repo_with_pushed_ai_feature_branch(crate::lines!["AI feature line".ai()]);

    // A colleague lands an unrelated commit on main, then the provider
    // squash-merges the PR on top of it — all with plain git in a scratch
    // clone, the way it happens server-side.
    let scratch = scratch_clone_of(&upstream);
    std::fs::write(scratch.join("colleague.txt"), "colleague change\n").unwrap();
    run_real_git_in(&scratch, &["add", "colleague.txt"]);
    run_real_git_in(&scratch, &["commit", "-m", "Unrelated colleague change"]);
    run_real_git_in(&scratch, &["fetch", "origin", "feature"]);
    run_real_git_in(&scratch, &["merge", "--squash", "origin/feature"]);
    run_real_git_in(&scratch, &["commit", "-m", "Add AI feature (#2)"]);
    let colleague_sha = run_real_git_in(&scratch, &["rev-parse", "HEAD~1"]);
    let squash_sha = run_real_git_in(&scratch, &["rev-parse", "HEAD"]);
    run_real_git_in(
        &scratch,
        &["push", "origin", &format!("HEAD:{}", default_branch)],
    );
    let _ = std::fs::remove_dir_all(&scratch);

    local.git(&["config", "pull.rebase", "false"]).unwrap();
    local.git(&["pull"]).unwrap();

    assert!(
        local.read_authorship_note(&squash_sha).is_some(),
        "squash commit should get a reconstructed authorship note even though main advanced after the branch point"
    );
    assert!(
        local.read_authorship_note(&colleague_sha).is_none(),
        "an ordinary pulled commit must not be mistaken for a squash merge"
    );

    let mut feature = local.filename("feature.txt");
    feature.assert_lines_and_blame(crate::lines!["AI feature line".ai()]);
}

/// When the squash commit already has an authorship note (e.g. written by
/// `autter ci` and fetched with the pull), recovery must leave it untouched.
#[test]
fn test_pull_does_not_overwrite_existing_squash_note() {
    let (local, upstream, default_branch) =
        setup_repo_with_pushed_ai_feature_branch(crate::lines!["AI feature line".ai()]);

    let feature_tree =
        run_real_git_in(upstream.path(), &["rev-parse", "refs/heads/feature^{tree}"]);
    let main_ref = format!("refs/heads/{}", default_branch);
    let main_sha = run_real_git_in(upstream.path(), &["rev-parse", &main_ref]);
    let squash_sha = run_real_git_in(
        upstream.path(),
        &[
            "commit-tree",
            &feature_tree,
            "-p",
            &main_sha,
            "-m",
            "Add AI feature (#3)",
        ],
    );
    run_real_git_in(upstream.path(), &["update-ref", &main_ref, &squash_sha]);

    // Simulate CI having already produced a note for the squash commit: make
    // the commit object available locally, then attach a marker note.
    local.git(&["fetch", "origin"]).unwrap();
    local
        .git(&[
            "notes",
            "--ref=ai",
            "add",
            "-m",
            "{\"marker\":\"pre-existing note\"}",
            &squash_sha,
        ])
        .unwrap();

    local.git(&["config", "pull.rebase", "false"]).unwrap();
    local.git(&["pull"]).unwrap();

    let note = local
        .read_authorship_note(&squash_sha)
        .expect("squash commit should still have a note after pull");
    assert!(
        note.contains("pre-existing note"),
        "recovery must not overwrite an existing authorship note, got: {}",
        note
    );
}

crate::reuse_tests_in_worktree!(
    test_pull_recovers_authorship_after_remote_squash_merge,
    test_pull_recovers_authorship_when_base_advanced_after_branch,
    test_pull_does_not_overwrite_existing_squash_note,
);
