//! Regression tests: AI attribution must survive history rewrites that used to
//! drop it.
//!
//! - `git cherry-pick A..B` (a range) maps every picked commit to its source.
//! - `git cherry-pick -n` followed by a plain `git commit` carries the source's
//!   attribution into that commit.
//! - A conflicted cherry-pick finished with a plain `git commit` (instead of
//!   `git cherry-pick --continue`) keeps the source's attribution.
//! - `git cherry-pick -n` onto a tree with uncommitted AI work keeps both the
//!   picked lines' attribution and the live work's, in other files and in the
//!   same file.
//! - A conflicted cherry-pick persists what it was picking, so `--continue`
//!   still works after the background service restarts.
//! - A rebase onto a branch that renamed the file moves attribution to the new
//!   path.
//! - A rebase of commits that rename the file themselves keeps attribution on
//!   the commits before the rename: also when the file is edited again after
//!   the rename, renamed more than once, when its old path is reused by a new
//!   file, and when an interactive rebase folds the rename into the AI commit.
//!   So does an interactive rebase that drops the rename, and a second rebase
//!   of the already rebased branch.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn head_sha(repo: &TestRepo) -> String {
    repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string()
}

/// An edit the way an agent makes it: a checkpoint for whatever changed before
/// the agent touched the file, the write, then the AI checkpoint.
fn ai_edit(repo: &TestRepo, file: &str, contents: &str) {
    repo.autter(&["checkpoint", "human", file]).unwrap();
    fs::write(repo.path().join(file), contents).unwrap();
    repo.autter(&["checkpoint", "mock_ai", file]).unwrap();
}

/// The commits on HEAD that are not on `base`, oldest first.
fn commits_since(repo: &TestRepo, base: &str) -> Vec<String> {
    repo.git(&["rev-list", "--reverse", &format!("{}..HEAD", base)])
        .unwrap()
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Check out one rebased commit, so the usual helpers assert the files as that
/// commit has them. Attribution has to be right in every commit of a rebased
/// range, not only at its tip.
fn checkout_detached(repo: &TestRepo, commit: &str) {
    repo.git(&["checkout", "--detach", commit]).unwrap();
}

/// `a.txt` with two base lines on the default branch, and a `feature` branch
/// checked out from it. Returns the default branch's name.
fn base_file_and_feature_branch(repo: &TestRepo) -> String {
    fs::write(repo.path().join("a.txt"), "Base line 1\nBase line 2\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
    ]);
    let main_branch = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    main_branch
}

/// On the default branch, add a line at the top of `a.txt`: every line the
/// feature commits touch moves down, so a rebase has to recompute their notes.
/// Leaves `feature` checked out again.
fn add_main_header_to_base_file(repo: &TestRepo, main_branch: &str) {
    repo.git(&["checkout", main_branch]).unwrap();
    fs::write(
        repo.path().join("a.txt"),
        "Main header\nBase line 1\nBase line 2\n",
    )
    .unwrap();
    repo.stage_all_and_commit("Main edit").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
    ]);
    repo.git(&["checkout", "feature"]).unwrap();
}

#[test]
fn cherry_pick_range_maps_each_commit_to_its_source() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("range.txt");

    fs::write(&file_path, "Base line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("range.txt");
    file.assert_committed_lines(lines!["Base line".unattributed_human()]);
    let main_branch = repo.current_branch();
    let range_start = head_sha(&repo);

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    repo.autter(&["checkpoint", "human", "range.txt"]).unwrap();
    fs::write(&file_path, "Base line\nAI one\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "range.txt"])
        .unwrap();
    repo.stage_all_and_commit("AI one").unwrap();
    file.assert_committed_lines(lines!["Base line".unattributed_human(), "AI one".ai()]);

    repo.autter(&["checkpoint", "human", "range.txt"]).unwrap();
    fs::write(&file_path, "Base line\nAI one\nAI two\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "range.txt"])
        .unwrap();
    repo.stage_all_and_commit("AI two").unwrap();
    file.assert_committed_lines(lines![
        "Base line".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(repo.path().join("other.txt"), "Unrelated\n").unwrap();
    repo.stage_all_and_commit("Unrelated main commit").unwrap();
    let mut other = repo.filename("other.txt");
    other.assert_committed_lines(lines!["Unrelated".unattributed_human()]);

    let range = format!("{}..feature", range_start);
    repo.git(&["cherry-pick", &range]).unwrap();
    repo.sync_daemon();

    file.assert_committed_lines(lines![
        "Base line".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    // Both picked commits got a note of their own, not just the last one.
    let first_pick = repo
        .git(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();
    let first_note = repo
        .read_authorship_note(&first_pick)
        .expect("first picked commit should have an authorship note");
    assert!(
        first_note.contains("range.txt"),
        "first picked commit's note should attribute range.txt, got:\n{}",
        first_note
    );
}

#[test]
fn cherry_pick_no_commit_then_plain_commit_keeps_ai_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("nocommit.txt");

    fs::write(&file_path, "Base line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("nocommit.txt");
    file.assert_committed_lines(lines!["Base line".unattributed_human()]);
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    repo.autter(&["checkpoint", "human", "nocommit.txt"])
        .unwrap();
    fs::write(&file_path, "Base line\nAI picked line\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "nocommit.txt"])
        .unwrap();
    repo.stage_all_and_commit("AI change").unwrap();
    file.assert_committed_lines(lines![
        "Base line".unattributed_human(),
        "AI picked line".ai()
    ]);
    let source = head_sha(&repo);

    repo.git(&["checkout", &main_branch]).unwrap();
    file.assert_committed_lines(lines!["Base line".unattributed_human()]);

    repo.git(&["cherry-pick", "-n", &source]).unwrap();
    repo.commit("Picked without committing").unwrap();
    repo.sync_daemon();

    file.assert_committed_lines(lines![
        "Base line".unattributed_human(),
        "AI picked line".ai()
    ]);
}

#[test]
fn cherry_pick_no_commit_onto_uncommitted_ai_work_keeps_both() {
    let repo = TestRepo::new();
    let picked_path = repo.path().join("picked.txt");
    let live_path = repo.path().join("live.txt");

    fs::write(&picked_path, "Picked base\n").unwrap();
    fs::write(&live_path, "Live base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut picked = repo.filename("picked.txt");
    let mut live = repo.filename("live.txt");
    picked.assert_committed_lines(lines!["Picked base".unattributed_human()]);
    live.assert_committed_lines(lines!["Live base".unattributed_human()]);
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    repo.autter(&["checkpoint", "human", "picked.txt"]).unwrap();
    fs::write(&picked_path, "Picked base\nAI picked line\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "picked.txt"])
        .unwrap();
    repo.stage_all_and_commit("AI change to pick").unwrap();
    picked.assert_committed_lines(lines![
        "Picked base".unattributed_human(),
        "AI picked line".ai(),
    ]);
    let source = head_sha(&repo);

    repo.git(&["checkout", &main_branch]).unwrap();
    picked.assert_committed_lines(lines!["Picked base".unattributed_human()]);

    // Uncommitted AI work in another file, left unstaged.
    repo.autter(&["checkpoint", "human", "live.txt"]).unwrap();
    fs::write(&live_path, "Live base\nLive AI line\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "live.txt"]).unwrap();

    repo.git(&["cherry-pick", "-n", &source]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("Pick onto live AI work").unwrap();
    repo.sync_daemon();

    picked.assert_committed_lines(lines![
        "Picked base".unattributed_human(),
        "AI picked line".ai(),
    ]);
    live.assert_committed_lines(lines![
        "Live base".unattributed_human(),
        "Live AI line".ai()
    ]);
}

#[test]
fn cherry_pick_no_commit_onto_staged_ai_work_in_same_file_keeps_both() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("shared.txt");

    fs::write(&file_path, "Line 1\nLine 2\nLine 3\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("shared.txt");
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    repo.autter(&["checkpoint", "human", "shared.txt"]).unwrap();
    fs::write(&file_path, "Line 1\nLine 2\nLine 3\nPicked AI line\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "shared.txt"])
        .unwrap();
    repo.stage_all_and_commit("AI change to pick").unwrap();
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
        "Picked AI line".ai(),
    ]);
    let source = head_sha(&repo);

    repo.git(&["checkout", &main_branch]).unwrap();
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);

    // Uncommitted AI work in the same file. `cherry-pick -n` merges against
    // the index, so the live work is staged first.
    repo.autter(&["checkpoint", "human", "shared.txt"]).unwrap();
    fs::write(&file_path, "Live AI line\nLine 1\nLine 2\nLine 3\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "shared.txt"])
        .unwrap();
    repo.git(&["add", "shared.txt"]).unwrap();

    repo.git(&["cherry-pick", "-n", &source]).unwrap();
    assert_eq!(
        fs::read_to_string(&file_path).unwrap(),
        "Live AI line\nLine 1\nLine 2\nLine 3\nPicked AI line\n",
        "cherry-pick -n should merge the picked line into the live work"
    );
    repo.commit("Pick onto live AI work").unwrap();
    repo.sync_daemon();

    file.assert_committed_lines(lines![
        "Live AI line".ai(),
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
        "Picked AI line".ai(),
    ]);
}

/// Sets up a cherry-pick that stops on a conflict. Returns the repo, the
/// conflicted file's path and the source commit.
fn conflicted_cherry_pick(name: &str) -> (TestRepo, std::path::PathBuf, String) {
    let repo = TestRepo::new();
    let (file_path, source) = set_up_conflicted_cherry_pick(&repo, name);
    (repo, file_path, source)
}

/// Commits an AI change on a feature branch and a conflicting human change on
/// the main branch, then cherry-picks the AI change, which stops on the
/// conflict. Returns the conflicted file's path and the source commit.
fn set_up_conflicted_cherry_pick(repo: &TestRepo, name: &str) -> (std::path::PathBuf, String) {
    let file_path = repo.path().join(name);

    fs::write(&file_path, "Line 1\nLine 2\nLine 3\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename(name);
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    repo.autter(&["checkpoint", "human", name]).unwrap();
    fs::write(&file_path, "Line 1\nAI version\nLine 3\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", name]).unwrap();
    repo.stage_all_and_commit("AI change").unwrap();
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "AI version".ai(),
        "Line 3".unattributed_human(),
    ]);
    let source = head_sha(repo);

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(&file_path, "Line 1\nMain version\nLine 3\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", name])
        .unwrap();
    repo.stage_all_and_commit("Human change").unwrap();
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Main version".human(),
        "Line 3".unattributed_human(),
    ]);

    assert!(
        repo.git(&["cherry-pick", &source]).is_err(),
        "cherry-pick should stop on a conflict"
    );
    repo.sync_daemon();

    (file_path, source)
}

#[test]
fn cherry_pick_conflict_finished_with_plain_commit_keeps_ai_attribution() {
    let (repo, file_path, _source) = conflicted_cherry_pick("plain.txt");

    // Resolve by taking the picked (AI) version, then finish with `git commit`
    // instead of `git cherry-pick --continue`.
    fs::write(&file_path, "Line 1\nAI version\nLine 3\n").unwrap();
    repo.git(&["add", "plain.txt"]).unwrap();
    repo.commit("Finish pick with plain commit").unwrap();
    repo.sync_daemon();

    let mut file = repo.filename("plain.txt");
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "AI version".ai(),
        "Line 3".unattributed_human(),
    ]);
}

#[test]
fn cherry_pick_conflict_persists_sources_for_continue() {
    let (repo, file_path, source) = conflicted_cherry_pick("persist.txt");

    // The sources of the stopped pick are on disk, so `--continue` does not
    // depend on the background service's memory surviving until then.
    let git_dir = repo
        .git(&["rev-parse", "--absolute-git-dir"])
        .unwrap()
        .trim()
        .to_string();
    let pending_path = std::path::Path::new(&git_dir)
        .join("ai")
        .join("pending_cherry_pick_sources.json");
    let pending = fs::read_to_string(&pending_path).unwrap_or_else(|err| {
        panic!(
            "pending cherry-pick sources should be persisted at {}: {}",
            pending_path.display(),
            err
        )
    });
    assert!(
        pending.contains(&source),
        "persisted sources should name the picked commit {}, got {}",
        source,
        pending
    );

    fs::write(&file_path, "Line 1\nAI version\nLine 3\n").unwrap();
    repo.git(&["add", "persist.txt"]).unwrap();
    repo.git(&["cherry-pick", "--continue"]).unwrap();
    repo.sync_daemon();

    let mut file = repo.filename("persist.txt");
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "AI version".ai(),
        "Line 3".unattributed_human(),
    ]);
    assert!(
        !pending_path.exists(),
        "persisted cherry-pick sources should be removed once the pick completes"
    );
}

#[test]
fn cherry_pick_continue_after_daemon_restart_keeps_ai_attribution() {
    let mut repo = TestRepo::new_dedicated_daemon();
    let (file_path, _source) = set_up_conflicted_cherry_pick(&repo, "restart.txt");

    // The service that saw the conflict goes away. The new one has nothing in
    // memory about the stopped pick and has to read it back from disk.
    repo.restart_dedicated_daemon();

    fs::write(&file_path, "Line 1\nAI version\nLine 3\n").unwrap();
    repo.git(&["add", "restart.txt"]).unwrap();
    repo.git(&["cherry-pick", "--continue"]).unwrap();
    repo.sync_daemon();

    let mut file = repo.filename("restart.txt");
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "AI version".ai(),
        "Line 3".unattributed_human(),
    ]);
}

#[test]
fn rebase_onto_renamed_file_moves_attribution_to_new_path() {
    let repo = TestRepo::new();
    let old_path = repo.path().join("old_name.txt");

    fs::write(&old_path, "Base line 1\nBase line 2\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut old_file = repo.filename("old_name.txt");
    old_file.assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
    ]);
    let main_branch = repo.current_branch();

    // Feature branch: the AI edits the file under its old name.
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    repo.autter(&["checkpoint", "human", "old_name.txt"])
        .unwrap();
    fs::write(&old_path, "Base line 1\nBase line 2\nAI added line\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "old_name.txt"])
        .unwrap();
    repo.stage_all_and_commit("AI edit").unwrap();
    old_file.assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI added line".ai(),
    ]);

    // Main renames the file.
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["mv", "old_name.txt", "new_name.txt"]).unwrap();
    repo.commit("Rename file").unwrap();
    let mut new_file = repo.filename("new_name.txt");
    new_file.assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
    ]);

    // Rebasing the feature onto main replays the AI edit onto the new path.
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();
    repo.sync_daemon();

    assert!(
        !old_path.exists(),
        "rebase should have applied the AI edit to the renamed file"
    );
    new_file.assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI added line".ai(),
    ]);
}

#[test]
fn rebase_of_commits_that_rename_the_file_keeps_ai_attribution() {
    let repo = TestRepo::new();
    let old_path = repo.path().join("before.txt");

    fs::write(&old_path, "Base line 1\nBase line 2\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut old_file = repo.filename("before.txt");
    old_file.assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
    ]);
    let main_branch = repo.current_branch();

    // Feature branch: the AI edits the file, then a later commit renames it.
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    repo.autter(&["checkpoint", "human", "before.txt"]).unwrap();
    fs::write(&old_path, "Base line 1\nBase line 2\nAI added line\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "before.txt"])
        .unwrap();
    repo.stage_all_and_commit("AI edit").unwrap();
    old_file.assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI added line".ai(),
    ]);

    repo.git(&["mv", "before.txt", "after.txt"]).unwrap();
    repo.commit("Rename file").unwrap();
    let mut new_file = repo.filename("after.txt");
    new_file.assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI added line".ai(),
    ]);

    // Main adds a line at the top of the file, which shifts every line the
    // rebased commits touch, so their notes have to be recomputed.
    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(&old_path, "Main header\nBase line 1\nBase line 2\n").unwrap();
    repo.stage_all_and_commit("Main edit").unwrap();
    old_file.assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
    ]);

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();
    repo.sync_daemon();

    assert!(
        !old_path.exists(),
        "the rebased rename should remove before.txt"
    );
    new_file.assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI added line".ai(),
    ]);
}

#[test]
fn rebase_keeps_ai_attribution_when_the_file_is_edited_again_after_its_rename() {
    let repo = TestRepo::new();
    let main_branch = base_file_and_feature_branch(&repo);

    // The AI edits the file, a commit renames it, the AI edits it again.
    ai_edit(&repo, "a.txt", "Base line 1\nBase line 2\nAI one\n");
    repo.stage_all_and_commit("AI edit").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["mv", "a.txt", "b.txt"]).unwrap();
    repo.commit("Rename file").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    ai_edit(&repo, "b.txt", "Base line 1\nBase line 2\nAI one\nAI two\n");
    repo.stage_all_and_commit("AI edit after rename").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    add_main_header_to_base_file(&repo, &main_branch);
    repo.git(&["rebase", &main_branch]).unwrap();
    repo.sync_daemon();

    let rebased = commits_since(&repo, &main_branch);
    assert_eq!(rebased.len(), 3, "the rebase should replay three commits");

    checkout_detached(&repo, &rebased[0]);
    repo.filename("a.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    checkout_detached(&repo, &rebased[1]);
    assert!(!repo.path().join("a.txt").exists());
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["checkout", "feature"]).unwrap();
    assert_eq!(head_sha(&repo), rebased[2]);
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);
}

#[test]
fn rebase_keeps_ai_attribution_when_one_commit_renames_and_edits_the_file() {
    let repo = TestRepo::new();
    let main_branch = base_file_and_feature_branch(&repo);

    ai_edit(&repo, "a.txt", "Base line 1\nBase line 2\nAI one\n");
    repo.stage_all_and_commit("AI edit").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    // One commit both renames the file and carries an AI edit to it.
    repo.git(&["mv", "a.txt", "b.txt"]).unwrap();
    ai_edit(&repo, "b.txt", "Base line 1\nBase line 2\nAI one\nAI two\n");
    repo.stage_all_and_commit("Rename and AI edit").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    add_main_header_to_base_file(&repo, &main_branch);
    repo.git(&["rebase", &main_branch]).unwrap();
    repo.sync_daemon();

    let rebased = commits_since(&repo, &main_branch);
    assert_eq!(rebased.len(), 2, "the rebase should replay two commits");

    checkout_detached(&repo, &rebased[0]);
    repo.filename("a.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["checkout", "feature"]).unwrap();
    assert_eq!(head_sha(&repo), rebased[1]);
    assert!(!repo.path().join("a.txt").exists());
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);
}

#[test]
fn rebase_keeps_ai_attribution_through_a_chain_of_renames() {
    let repo = TestRepo::new();
    let main_branch = base_file_and_feature_branch(&repo);

    // a.txt -> b.txt -> c.txt, with an AI edit under each name.
    ai_edit(&repo, "a.txt", "Base line 1\nBase line 2\nAI one\n");
    repo.stage_all_and_commit("AI edit 1").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["mv", "a.txt", "b.txt"]).unwrap();
    repo.commit("Rename a to b").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    ai_edit(&repo, "b.txt", "Base line 1\nBase line 2\nAI one\nAI two\n");
    repo.stage_all_and_commit("AI edit 2").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    repo.git(&["mv", "b.txt", "c.txt"]).unwrap();
    repo.commit("Rename b to c").unwrap();
    repo.filename("c.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    ai_edit(
        &repo,
        "c.txt",
        "Base line 1\nBase line 2\nAI one\nAI two\nAI three\n",
    );
    repo.stage_all_and_commit("AI edit 3").unwrap();
    repo.filename("c.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
        "AI three".ai(),
    ]);

    add_main_header_to_base_file(&repo, &main_branch);
    repo.git(&["rebase", &main_branch]).unwrap();
    repo.sync_daemon();

    let rebased = commits_since(&repo, &main_branch);
    assert_eq!(rebased.len(), 5, "the rebase should replay five commits");

    checkout_detached(&repo, &rebased[0]);
    repo.filename("a.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    checkout_detached(&repo, &rebased[1]);
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    checkout_detached(&repo, &rebased[2]);
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    checkout_detached(&repo, &rebased[3]);
    repo.filename("c.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    repo.git(&["checkout", "feature"]).unwrap();
    assert_eq!(head_sha(&repo), rebased[4]);
    assert!(!repo.path().join("a.txt").exists());
    assert!(!repo.path().join("b.txt").exists());
    repo.filename("c.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
        "AI three".ai(),
    ]);
}

#[test]
fn rebase_keeps_ai_attribution_when_a_new_file_reuses_the_renamed_files_old_path() {
    let repo = TestRepo::new();
    let main_branch = base_file_and_feature_branch(&repo);

    ai_edit(&repo, "a.txt", "Base line 1\nBase line 2\nAI one\n");
    repo.stage_all_and_commit("AI edit").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["mv", "a.txt", "b.txt"]).unwrap();
    repo.commit("Rename file").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    // A later commit creates a new, unrelated file at the old path. No
    // checkpoint: nothing recorded who wrote it.
    fs::write(
        repo.path().join("a.txt"),
        "Brand new file\nat the old path\n",
    )
    .unwrap();
    repo.stage_all_and_commit("Reuse the old path").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Brand new file".unattributed_human(),
        "at the old path".unattributed_human(),
    ]);
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    add_main_header_to_base_file(&repo, &main_branch);
    repo.git(&["rebase", &main_branch]).unwrap();
    repo.sync_daemon();

    let rebased = commits_since(&repo, &main_branch);
    assert_eq!(rebased.len(), 3, "the rebase should replay three commits");

    checkout_detached(&repo, &rebased[0]);
    repo.filename("a.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    checkout_detached(&repo, &rebased[1]);
    assert!(!repo.path().join("a.txt").exists());
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["checkout", "feature"]).unwrap();
    assert_eq!(head_sha(&repo), rebased[2]);
    repo.filename("a.txt").assert_committed_lines(lines![
        "Brand new file".unattributed_human(),
        "at the old path".unattributed_human(),
    ]);
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);
}

#[test]
fn interactive_rebase_that_folds_the_rename_into_the_ai_commit_keeps_ai_attribution() {
    let repo = TestRepo::new();
    let main_branch = base_file_and_feature_branch(&repo);

    ai_edit(&repo, "a.txt", "Base line 1\nBase line 2\nAI one\n");
    repo.stage_all_and_commit("AI edit").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["mv", "a.txt", "b.txt"]).unwrap();
    repo.commit("Rename file").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    // An edit nothing recorded a checkpoint for.
    fs::write(
        repo.path().join("b.txt"),
        "Base line 1\nBase line 2\nAI one\nHuman tail\n",
    )
    .unwrap();
    repo.stage_all_and_commit("Human edit").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "Human tail".unattributed_human(),
    ]);

    add_main_header_to_base_file(&repo, &main_branch);

    // Make the rename commit a fixup of the AI commit: the commit that adds
    // the AI line now also renames the file.
    repo.git_with_env(
        &["rebase", "-i", &main_branch],
        &[
            (
                "GIT_SEQUENCE_EDITOR",
                "perl -i -pe 's/^pick/fixup/ if $. == 2'",
            ),
            ("GIT_EDITOR", "true"),
        ],
        None,
    )
    .unwrap();
    repo.sync_daemon();

    let rebased = commits_since(&repo, &main_branch);
    assert_eq!(rebased.len(), 2, "the fixup should leave two commits");

    checkout_detached(&repo, &rebased[0]);
    assert!(
        !repo.path().join("a.txt").exists(),
        "the AI commit should now contain the rename"
    );
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);
    let note = repo
        .read_authorship_note(&rebased[0])
        .expect("the AI commit should have a note");
    assert!(
        note.lines().any(|line| line == "b.txt") && !note.lines().any(|line| line == "a.txt"),
        "the note should attest the file under the name the commit gives it:\n{}",
        note
    );

    repo.git(&["checkout", "feature"]).unwrap();
    assert_eq!(head_sha(&repo), rebased[1]);
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "Human tail".unattributed_human(),
    ]);
}

#[test]
fn interactive_rebase_that_drops_the_rename_keeps_ai_attribution() {
    let repo = TestRepo::new();
    let main_branch = base_file_and_feature_branch(&repo);

    ai_edit(&repo, "a.txt", "Base line 1\nBase line 2\nAI one\n");
    repo.stage_all_and_commit("AI edit").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["mv", "a.txt", "b.txt"]).unwrap();
    repo.commit("Rename file").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    ai_edit(&repo, "b.txt", "Base line 1\nBase line 2\nAI one\nAI two\n");
    repo.stage_all_and_commit("AI edit after rename").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    add_main_header_to_base_file(&repo, &main_branch);

    // Drop the rename commit. Git applies the later AI edit to a.txt, so the
    // rebased branch never has a b.txt, while every original note after the
    // rename names b.txt.
    repo.git_with_env(
        &["rebase", "-i", &main_branch],
        &[
            ("GIT_SEQUENCE_EDITOR", "perl -i -ne 'print unless $. == 2'"),
            ("GIT_EDITOR", "true"),
        ],
        None,
    )
    .unwrap();
    repo.sync_daemon();

    let rebased = commits_since(&repo, &main_branch);
    assert_eq!(
        rebased.len(),
        2,
        "dropping the rename should leave two commits"
    );

    checkout_detached(&repo, &rebased[0]);
    repo.filename("a.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["checkout", "feature"]).unwrap();
    assert_eq!(head_sha(&repo), rebased[1]);
    assert!(
        !repo.path().join("b.txt").exists(),
        "without the rename the file keeps its old name"
    );
    repo.filename("a.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);
}

#[test]
fn rebasing_a_renaming_branch_twice_keeps_ai_attribution() {
    let repo = TestRepo::new();
    let main_branch = base_file_and_feature_branch(&repo);

    ai_edit(&repo, "a.txt", "Base line 1\nBase line 2\nAI one\n");
    repo.stage_all_and_commit("AI edit").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["mv", "a.txt", "b.txt"]).unwrap();
    repo.commit("Rename file").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    ai_edit(&repo, "b.txt", "Base line 1\nBase line 2\nAI one\nAI two\n");
    repo.stage_all_and_commit("AI edit after rename").unwrap();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    add_main_header_to_base_file(&repo, &main_branch);
    repo.git(&["rebase", &main_branch]).unwrap();
    repo.sync_daemon();
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    // Main moves again; the second rebase starts from the notes the first one
    // wrote.
    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(
        repo.path().join("a.txt"),
        "Main header 2\nMain header\nBase line 1\nBase line 2\n",
    )
    .unwrap();
    repo.stage_all_and_commit("Second main edit").unwrap();
    repo.filename("a.txt").assert_committed_lines(lines![
        "Main header 2".unattributed_human(),
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
    ]);
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();
    repo.sync_daemon();

    let rebased = commits_since(&repo, &main_branch);
    assert_eq!(rebased.len(), 3, "the rebase should replay three commits");

    checkout_detached(&repo, &rebased[0]);
    repo.filename("a.txt").assert_committed_lines(lines![
        "Main header 2".unattributed_human(),
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    checkout_detached(&repo, &rebased[1]);
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header 2".unattributed_human(),
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
    ]);

    repo.git(&["checkout", "feature"]).unwrap();
    assert_eq!(head_sha(&repo), rebased[2]);
    repo.filename("b.txt").assert_committed_lines(lines![
        "Main header 2".unattributed_human(),
        "Main header".unattributed_human(),
        "Base line 1".unattributed_human(),
        "Base line 2".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);
}
