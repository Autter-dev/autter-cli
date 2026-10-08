//! End-to-end CI scenarios for attribution across server-side history rewrites,
//! shallow clones, and clones that do not carry `refs/notes/ai`.
//!
//! The "server" is a bare repository reached through a `file://` URL. Server-side
//! merges (GitHub "Squash and merge" / "Rebase and merge") are simulated with plain
//! git in an isolated clone, so the new commits never pass through autter hooks,
//! exactly as the CI handler sees them. `autter ci github run` is then driven with a
//! `pull_request` `closed` payload, the same way `ci_handlers_comprehensive.rs`
//! drives the `synchronize` path.

use crate::history_rewrite_attribution::ai_lines_in_note;
use crate::repos::test_repo::{TestRepo, real_git_executable};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const PR_NUMBER: u32 = 42;

/// Run plain git with no global/system config (no autter hooks or trace2), so
/// commits made here look like commits created by the hosting provider.
fn server_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(real_git_executable())
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=GitHub",
            "-c",
            "user.email=noreply@github.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("failed to spawn git");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!("git {:?} failed:\n{}\n{}", args, stdout, stderr))
    }
}

fn server_git_ok(dir: &Path, args: &[&str]) -> String {
    server_git(dir, args).unwrap_or_else(|e| panic!("{e}"))
}

fn file_url(path: &Path) -> String {
    url::Url::from_directory_path(path)
        .expect("absolute path")
        .to_string()
}

fn ai_edit(repo: &TestRepo, rel: &str, content: &str) {
    repo.autter(&["checkpoint", "human", rel]).unwrap();
    fs::write(repo.path().join(rel), content).unwrap();
    repo.autter(&["checkpoint", "mock_ai", rel]).unwrap();
}

fn human_edit(repo: &TestRepo, rel: &str, content: &str) {
    fs::write(repo.path().join(rel), content).unwrap();
    repo.autter(&["checkpoint", "mock_known_human", rel])
        .unwrap();
}

fn commit_all(repo: &TestRepo, message: &str) -> String {
    repo.git(&["add", "-A"]).unwrap();
    repo.commit(message).unwrap().commit_sha
}

fn remote_note(upstream: &TestRepo, sha: &str) -> Option<String> {
    server_git(upstream.path(), &["notes", "--ref=ai", "show", sha])
        .ok()
        .filter(|n| !n.trim().is_empty())
}

/// A PR pushed to a bare "server" with notes for its commits.
struct PrFixture {
    local: TestRepo,
    upstream: TestRepo,
    base_sha: String,
    head_sha: String,
    pr_commits: Vec<String>,
    _scratch: tempfile::TempDir,
}

impl PrFixture {
    /// main: app.txt (human). feature: two commits with AI lines in feature.txt
    /// and a human line in app.txt. Branch, `refs/pull/N/head` and notes are pushed.
    fn new() -> Self {
        let (local, upstream) = TestRepo::new_with_remote();
        human_edit(&local, "app.txt", "fn main() {}\n");
        let base_sha = commit_all(&local, "base");
        local.git_og(&["push", "origin", "HEAD:main"]).unwrap();

        local.git(&["checkout", "-b", "feature"]).unwrap();
        ai_edit(&local, "feature.txt", "ai 1\nai 2\n");
        let c1 = commit_all(&local, "AI commit 1");
        human_edit(&local, "app.txt", "fn main() {}\n// human note\n");
        ai_edit(&local, "feature.txt", "ai 1\nai 2\nai 3\n");
        let c2 = commit_all(&local, "AI commit 2");

        local.sync_daemon();
        assert!(local.read_authorship_note(&c1).is_some());
        assert!(local.read_authorship_note(&c2).is_some());
        local.git_og(&["push", "origin", "feature"]).unwrap();
        local
            .git_og(&[
                "push",
                "origin",
                &format!("feature:refs/pull/{PR_NUMBER}/head"),
            ])
            .unwrap();
        local
            .git_og(&["push", "origin", "+refs/notes/ai:refs/notes/ai"])
            .unwrap();

        Self {
            local,
            upstream,
            base_sha,
            head_sha: c2.clone(),
            pr_commits: vec![c1, c2],
            _scratch: tempfile::tempdir().unwrap(),
        }
    }

    fn server_workdir(&self) -> PathBuf {
        let dir = self._scratch.path().join("server-work");
        if !dir.exists() {
            server_git_ok(
                self._scratch.path(),
                &[
                    "clone",
                    self.upstream.path().to_str().unwrap(),
                    dir.to_str().unwrap(),
                ],
            );
        }
        dir
    }

    /// Someone else's PR lands on main after this PR was opened (base.sha goes stale).
    fn advance_main_on_server(&self) -> String {
        let work = self.server_workdir();
        server_git_ok(&work, &["checkout", "main"]);
        fs::write(work.join("other.txt"), "landed from another PR\n").unwrap();
        server_git_ok(&work, &["add", "other.txt"]);
        server_git_ok(&work, &["commit", "-m", "Other PR (#41)"]);
        server_git_ok(&work, &["push", "origin", "main"]);
        server_git_ok(&work, &["rev-parse", "HEAD"])
    }

    /// GitHub "Squash and merge", without autter hooks.
    fn squash_merge_on_server(&self) -> String {
        let work = self.server_workdir();
        server_git_ok(&work, &["fetch", "origin"]);
        server_git_ok(&work, &["checkout", "main"]);
        server_git_ok(&work, &["merge", "--squash", "origin/feature"]);
        server_git_ok(&work, &["commit", "-m", &format!("Feature (#{PR_NUMBER})")]);
        server_git_ok(&work, &["push", "origin", "main"]);
        server_git_ok(&work, &["rev-parse", "HEAD"])
    }

    /// GitHub "Rebase and merge", without autter hooks. Returns the new commits, oldest first.
    fn rebase_merge_on_server(&self) -> Vec<String> {
        let work = self.server_workdir();
        server_git_ok(&work, &["fetch", "origin"]);
        server_git_ok(&work, &["checkout", "-B", "rebase-tmp", "origin/feature"]);
        server_git_ok(&work, &["rebase", "origin/main"]);
        let shas = server_git_ok(&work, &["rev-list", "--reverse", "origin/main..HEAD"]);
        server_git_ok(&work, &["push", "origin", "HEAD:main"]);
        shas.lines().map(str::to_string).collect()
    }

    fn merged_event(&self, merge_commit_sha: &str) -> serde_json::Value {
        let url = file_url(self.upstream.path());
        serde_json::json!({
            "action": "closed",
            "pull_request": {
                "number": PR_NUMBER,
                "merged": true,
                "merge_commit_sha": merge_commit_sha,
                "base": { "ref": "main", "sha": self.base_sha, "repo": { "clone_url": url } },
                "head": { "ref": "feature", "sha": self.head_sha, "repo": { "clone_url": url } }
            }
        })
    }
}

fn run_github_ci(runner: &TestRepo, event: &serde_json::Value) -> Result<String, String> {
    let mut event_file = tempfile::NamedTempFile::new().unwrap();
    serde_json::to_writer(&mut event_file, event).unwrap();
    event_file.flush().unwrap();
    runner.autter_with_env(
        &["ci", "github", "run", "--no-cleanup"],
        &[
            ("GITHUB_EVENT_NAME", "pull_request"),
            ("GITHUB_EVENT_PATH", event_file.path().to_str().unwrap()),
        ],
    )
}

/// Clone the server the way `actions/checkout` would (`fetch-depth: 1` by default).
fn checkout_workspace(upstream: &TestRepo, depth: Option<u32>, dir: &Path) {
    let url = file_url(upstream.path());
    let mut args = vec!["clone".to_string()];
    if let Some(depth) = depth {
        args.push(format!("--depth={depth}"));
    }
    args.push(url);
    args.push(dir.to_str().unwrap().to_string());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    server_git_ok(dir.parent().unwrap(), &args);
}

fn is_shallow(dir: &Path) -> bool {
    server_git_ok(dir, &["rev-parse", "--is-shallow-repository"]) == "true"
}

// ---------------------------------------------------------------------------
// Server-side squash / rebase merge, reconstructed by `autter ci github run`
// ---------------------------------------------------------------------------

#[test]
fn test_ci_github_run_reconstructs_server_side_squash_merge() {
    let pr = PrFixture::new();
    let merge_sha = pr.squash_merge_on_server();
    assert!(remote_note(&pr.upstream, &merge_sha).is_none());

    let runner = TestRepo::new();
    let output = run_github_ci(&runner, &pr.merged_event(&merge_sha))
        .unwrap_or_else(|e| panic!("ci github run failed:\n{e}"));
    assert!(output.contains("Fetched authorship history"), "{output}");

    let note = remote_note(&pr.upstream, &merge_sha)
        .unwrap_or_else(|| panic!("squash commit should have a pushed note\n{output}"));
    assert_eq!(ai_lines_in_note(&note, "feature.txt"), vec![1, 2, 3]);
    assert!(ai_lines_in_note(&note, "app.txt").is_empty());
}

#[test]
fn test_ci_github_run_reconstructs_server_side_squash_merge_after_base_advanced() {
    // base.sha in the payload is the main tip when the PR was opened. Another PR
    // lands first, so `base_sha..merge` contains 2 commits, the same count as the PR.
    let pr = PrFixture::new();
    let other_pr_sha = pr.advance_main_on_server();
    let merge_sha = pr.squash_merge_on_server();

    let runner = TestRepo::new();
    let output = run_github_ci(&runner, &pr.merged_event(&merge_sha))
        .unwrap_or_else(|e| panic!("ci github run failed:\n{e}"));

    assert!(
        remote_note(&pr.upstream, &other_pr_sha).is_none(),
        "an unrelated commit on main must not receive this PR's attribution\n{output}"
    );
    let note = remote_note(&pr.upstream, &merge_sha)
        .unwrap_or_else(|| panic!("squash commit should have a pushed note\n{output}"));
    assert_eq!(
        ai_lines_in_note(&note, "feature.txt"),
        vec![1, 2, 3],
        "squash commit should carry the combined attribution\n{output}"
    );
}

#[test]
fn test_ci_github_run_reconstructs_server_side_rebase_merge() {
    let pr = PrFixture::new();
    pr.advance_main_on_server();
    let new_commits = pr.rebase_merge_on_server();
    assert_eq!(new_commits.len(), 2);
    for sha in &new_commits {
        assert!(
            !pr.pr_commits.contains(sha),
            "rebase merge should rewrite SHAs"
        );
    }

    let runner = TestRepo::new();
    let output = run_github_ci(&runner, &pr.merged_event(new_commits.last().unwrap()))
        .unwrap_or_else(|e| panic!("ci github run failed:\n{e}"));
    assert!(output.contains("Detected rebase merge"), "{output}");

    for (original, rewritten) in pr.pr_commits.iter().zip(&new_commits) {
        let expected = ai_lines_in_note(
            &pr.local.read_authorship_note(original).unwrap(),
            "feature.txt",
        );
        let note = remote_note(&pr.upstream, rewritten)
            .unwrap_or_else(|| panic!("rebased {rewritten} should have a pushed note\n{output}"));
        assert_eq!(ai_lines_in_note(&note, "feature.txt"), expected);
    }
}

// ---------------------------------------------------------------------------
// Shallow clones in CI
// ---------------------------------------------------------------------------

/// `autter ci github run` invoked from an `actions/checkout` workspace. The
/// command clones its own copy of the repo, so the workspace depth must not matter.
fn github_run_from_checkout_workspace(depth: Option<u32>) {
    let pr = PrFixture::new();
    let merge_sha = pr.squash_merge_on_server();

    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().join("workspace");
    checkout_workspace(&pr.upstream, depth, &workspace);
    assert_eq!(is_shallow(&workspace), depth.is_some());

    let runner = TestRepo::new_at_path(&workspace);
    let output = run_github_ci(&runner, &pr.merged_event(&merge_sha))
        .unwrap_or_else(|e| panic!("ci github run failed (depth {depth:?}):\n{e}"));

    assert!(!is_shallow(&workspace.join("autter-ci-clone")));
    let note = remote_note(&pr.upstream, &merge_sha)
        .unwrap_or_else(|| panic!("squash commit should have a pushed note\n{output}"));
    assert_eq!(ai_lines_in_note(&note, "feature.txt"), vec![1, 2, 3]);
}

#[test]
fn test_ci_github_run_from_shallow_checkout_workspace_depth_1() {
    github_run_from_checkout_workspace(Some(1));
}

#[test]
fn test_ci_github_run_from_full_checkout_workspace_depth_0_control() {
    github_run_from_checkout_workspace(None);
}

/// `autter ci local merge` operates on the current repo. Run it in a workspace
/// checked out like `actions/checkout` and pass it the PR coordinates.
fn ci_local_merge_in_workspace(
    depth: Option<u32>,
) -> (PrFixture, tempfile::TempDir, String, Result<String, String>) {
    let pr = PrFixture::new();
    let merge_sha = pr.squash_merge_on_server();

    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().join("workspace");
    checkout_workspace(&pr.upstream, depth, &workspace);

    let runner = TestRepo::new_at_path(&workspace);
    let result = runner.autter(&[
        "ci",
        "local",
        "merge",
        "--merge-commit-sha",
        &merge_sha,
        "--base-ref",
        "main",
        "--head-ref",
        "feature",
        "--head-sha",
        &pr.head_sha,
        "--base-sha",
        &pr.base_sha,
    ]);
    (pr, scratch, merge_sha, result)
}

#[test]
fn test_ci_local_merge_in_full_checkout_control() {
    let (pr, _scratch, merge_sha, result) = ci_local_merge_in_workspace(None);
    let output = result.unwrap_or_else(|e| panic!("ci local merge failed:\n{e}"));
    let note = remote_note(&pr.upstream, &merge_sha)
        .unwrap_or_else(|| panic!("squash commit should have a pushed note\n{output}"));
    assert_eq!(ai_lines_in_note(&note, "feature.txt"), vec![1, 2, 3]);
}

#[test]
fn test_ci_local_merge_in_shallow_checkout_reconstructs_or_fails_clearly() {
    let (pr, _scratch, merge_sha, result) = ci_local_merge_in_workspace(Some(1));
    match result {
        Ok(output) => {
            let note = remote_note(&pr.upstream, &merge_sha).unwrap_or_else(|| {
                panic!("shallow ci local merge reported success but wrote no note:\n{output}")
            });
            assert_eq!(
                ai_lines_in_note(&note, "feature.txt"),
                vec![1, 2, 3],
                "{output}"
            );
        }
        Err(err) => {
            assert!(
                err.to_lowercase().contains("shallow"),
                "shallow ci local merge should fail with a message that names the shallow clone, got:\n{err}"
            );
            assert!(remote_note(&pr.upstream, &merge_sha).is_none());
        }
    }
}

// ---------------------------------------------------------------------------
// refs/notes/ai is not part of a default clone
// ---------------------------------------------------------------------------

fn plain_clone_then_fetch_notes(depth: Option<u32>) {
    let pr = PrFixture::new();
    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().join("plain-clone");
    // Clone the feature branch so the PR head is the tip even at depth 1.
    let url = file_url(pr.upstream.path());
    let mut args = vec!["clone", "--branch", "feature"];
    let depth_arg;
    if let Some(depth) = depth {
        depth_arg = format!("--depth={depth}");
        args.push(&depth_arg);
    }
    args.push(&url);
    args.push(workspace.to_str().unwrap());
    server_git_ok(scratch.path(), &args);

    assert!(
        server_git(&workspace, &["rev-parse", "--verify", "refs/notes/ai"]).is_err(),
        "a plain clone does not fetch refs/notes/ai"
    );

    let runner = TestRepo::new_at_path(&workspace);
    let output = runner
        .autter(&["fetch-notes"])
        .unwrap_or_else(|e| panic!("fetch-notes failed:\n{e}"));
    let note = runner
        .read_authorship_note(&pr.head_sha)
        .unwrap_or_else(|| panic!("fetch-notes should bring the PR head note\n{output}"));
    let expected = pr.local.read_authorship_note(&pr.head_sha).unwrap();
    assert_eq!(
        ai_lines_in_note(&note, "feature.txt"),
        ai_lines_in_note(&expected, "feature.txt")
    );
}

#[test]
fn test_plain_clone_lacks_notes_until_fetch_notes() {
    plain_clone_then_fetch_notes(None);
}

#[test]
fn test_shallow_plain_clone_fetch_notes_depth_1() {
    plain_clone_then_fetch_notes(Some(1));
}

#[test]
fn test_daemon_shallow_clone_with_depth_flag_fetches_notes() {
    // `git clone --depth 1 <url> <dir>` through autter: the clone target must be
    // resolved (not "1") so the post-clone notes sync runs against the new repo.
    let pr = PrFixture::new();
    let scratch = tempfile::tempdir().unwrap();
    let target = scratch.path().join("depth-clone");
    let url = file_url(pr.upstream.path());
    pr.local
        .git(&[
            "clone",
            "--depth",
            "1",
            // `-b`, not `--branch`: the test harness resolves the clone target with
            // `cli_parser::extract_clone_target_directory`, which only knows `-b`.
            "-b",
            "feature",
            &url,
            target.to_str().unwrap(),
        ])
        .expect("clone should succeed");
    assert!(is_shallow(&target));

    let clone = TestRepo::new_at_path(&target);
    assert!(
        clone.read_authorship_note(&pr.head_sha).is_some(),
        "autter clone --depth 1 should fetch refs/notes/ai for the cloned tip"
    );
}
