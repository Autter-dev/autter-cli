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
//! - A `git reset` that leaves HEAD where it is (no target, or only paths) is
//!   not a move from the branch checked out before, and keeps live AI work.
//! - Lines typed after switching back from a branch with AI commits, or after
//!   `git reset --hard` threw an AI commit away, are not that AI's.
//! - A conflicted cherry-pick persists what it was picking, so `--continue`
//!   still works after the background service restarts.
//! - A rebase onto a branch that renamed the file moves attribution to the new
//!   path.
//! - A rebase of commits that rename the file themselves keeps attribution on
//!   the commits before the rename.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn head_sha(repo: &TestRepo) -> String {
    repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string()
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

/// An AI agent's edit of one file: the "before" checkpoint every agent preset
/// takes, the write, and the "after" checkpoint.
fn ai_edit(repo: &TestRepo, name: &str, contents: &str) {
    repo.autter(&["checkpoint", "human", name]).unwrap();
    fs::write(repo.path().join(name), contents).unwrap();
    repo.autter(&["checkpoint", "mock_ai", name]).unwrap();
}

/// The rewrite events the background service recorded for the repository.
fn rewrite_log(repo: &TestRepo) -> String {
    let git_dir = repo
        .git(&["rev-parse", "--absolute-git-dir"])
        .unwrap()
        .trim()
        .to_string();
    fs::read_to_string(
        std::path::Path::new(&git_dir)
            .join("ai")
            .join("rewrite_log"),
    )
    .unwrap_or_default()
}

/// Commits a base `name`, then an AI commit on a `feature` branch that adds a
/// line to it, and returns to the main branch. Returns the AI commit.
fn ai_commit_on_visited_branch(repo: &TestRepo, name: &str) -> String {
    fs::write(repo.path().join(name), "Line 1\nLine 2\nLine 3\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename(name);
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    ai_edit(repo, name, "Line 1\nLine 2\nLine 3\nFeature AI line\n");
    repo.stage_all_and_commit("AI change on feature").unwrap();
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
        "Feature AI line".ai(),
    ]);
    let source = head_sha(repo);

    repo.git(&["checkout", &main_branch]).unwrap();
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);
    source
}

#[test]
fn reset_that_keeps_head_after_branch_visit_keeps_live_ai_attribution() {
    let repo = TestRepo::new();
    ai_commit_on_visited_branch(&repo, "unstage.txt");

    // Live AI work on main, staged and then unstaged again. `git reset` with no
    // target leaves HEAD where it is.
    ai_edit(
        &repo,
        "unstage.txt",
        "Live AI line\nLine 1\nLine 2\nLine 3\n",
    );
    repo.git(&["add", "unstage.txt"]).unwrap();
    repo.git(&["reset"]).unwrap();
    repo.stage_all_and_commit("Commit live AI work").unwrap();
    repo.sync_daemon();

    let mut file = repo.filename("unstage.txt");
    file.assert_committed_lines(lines![
        "Live AI line".ai(),
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);
    let log = rewrite_log(&repo);
    assert!(
        !log.contains("\"reset\""),
        "a reset that did not move HEAD must not be recorded as a move, got:\n{}",
        log
    );
}

#[test]
fn reset_of_paths_after_branch_visit_keeps_live_ai_attribution() {
    let repo = TestRepo::new();
    ai_commit_on_visited_branch(&repo, "paths.txt");

    // `git reset -- <path>` only touches the index.
    ai_edit(&repo, "paths.txt", "Live AI line\nLine 1\nLine 2\nLine 3\n");
    repo.git(&["add", "paths.txt"]).unwrap();
    repo.git(&["reset", "--", "paths.txt"]).unwrap();
    repo.stage_all_and_commit("Commit live AI work").unwrap();
    repo.sync_daemon();

    let mut file = repo.filename("paths.txt");
    file.assert_committed_lines(lines![
        "Live AI line".ai(),
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);
    let log = rewrite_log(&repo);
    assert!(
        !log.contains("\"reset\""),
        "a reset that did not move HEAD must not be recorded as a move, got:\n{}",
        log
    );
}

#[test]
fn reset_of_paths_after_undoing_a_commit_keeps_later_ai_work() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("undo.txt");

    fs::write(&file_path, "Line 1\nLine 2\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("undo.txt");
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
    ]);

    ai_edit(&repo, "undo.txt", "Line 1\nLine 2\nFirst AI line\n");
    repo.stage_all_and_commit("First AI commit").unwrap();
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "First AI line".ai(),
    ]);

    // Undo the commit, keep working, then unstage the file. The second reset
    // moves nothing and must not replay the first one over the newer work.
    repo.git(&["reset", "--soft", "HEAD~1"]).unwrap();
    ai_edit(
        &repo,
        "undo.txt",
        "Line 1\nLine 2\nFirst AI line\nSecond AI line\n",
    );
    repo.git(&["add", "undo.txt"]).unwrap();
    repo.git(&["reset", "--", "undo.txt"]).unwrap();
    repo.stage_all_and_commit("Both AI lines").unwrap();
    repo.sync_daemon();

    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "First AI line".ai(),
        "Second AI line".ai(),
    ]);
}

#[test]
fn cherry_pick_no_commit_onto_staged_ai_work_then_reset_keeps_both() {
    let repo = TestRepo::new();
    let source = ai_commit_on_visited_branch(&repo, "unstaged.txt");

    ai_edit(
        &repo,
        "unstaged.txt",
        "Live AI line\nLine 1\nLine 2\nLine 3\n",
    );
    repo.git(&["add", "unstaged.txt"]).unwrap();
    repo.git(&["cherry-pick", "-n", &source]).unwrap();
    assert_eq!(
        fs::read_to_string(repo.path().join("unstaged.txt")).unwrap(),
        "Live AI line\nLine 1\nLine 2\nLine 3\nFeature AI line\n",
        "cherry-pick -n should merge the picked line into the live work"
    );

    // Unstage everything, then commit it all again.
    repo.git(&["reset"]).unwrap();
    repo.stage_all_and_commit("Live work and pick").unwrap();
    repo.sync_daemon();

    let mut file = repo.filename("unstaged.txt");
    file.assert_committed_lines(lines![
        "Live AI line".ai(),
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
        "Feature AI line".ai(),
    ]);
}

#[test]
fn line_typed_after_visiting_a_branch_with_an_ai_commit_is_not_the_ais() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("visited.txt");

    fs::write(&file_path, "Line 1\nLine 2\nLine 3\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("visited.txt");
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);
    let main_branch = repo.current_branch();

    // The AI rewrites line 2 on a feature branch.
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    ai_edit(&repo, "visited.txt", "Line 1\nAI version\nLine 3\n");
    repo.stage_all_and_commit("AI change on feature").unwrap();
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "AI version".ai(),
        "Line 3".unattributed_human(),
    ]);

    // Back on main, the same line is rewritten in a plain editor, with some
    // of the AI's words: no checkpoint. Checking main out again is not a reset
    // that undid the feature commit, so nothing of that commit is "recovered"
    // into this one.
    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(&file_path, "Line 1\nMain version\nLine 3\n").unwrap();
    repo.stage_all_and_commit("Change on main").unwrap();
    repo.sync_daemon();

    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Main version".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);
}

#[test]
fn line_typed_after_hard_reset_of_an_ai_commit_is_not_the_ais() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("discarded.txt");

    fs::write(&file_path, "Line 1\nLine 2\nLine 3\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("discarded.txt");
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);

    ai_edit(&repo, "discarded.txt", "Line 1\nAI version\nLine 3\n");
    repo.stage_all_and_commit("AI change").unwrap();
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "AI version".ai(),
        "Line 3".unattributed_human(),
    ]);

    // Throw the AI commit away and write the line by hand instead, with some
    // of the AI's words.
    repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();
    fs::write(&file_path, "Line 1\nHand version\nLine 3\n").unwrap();
    repo.stage_all_and_commit("Change by hand").unwrap();
    repo.sync_daemon();

    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "Hand version".unattributed_human(),
        "Line 3".unattributed_human(),
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
