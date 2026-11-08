//! Regression tests: AI attribution must survive history rewrites that used to
//! drop it.
//!
//! - `git cherry-pick A..B` (a range) maps every picked commit to its source.
//! - `git cherry-pick -n` followed by a plain `git commit` carries the source's
//!   attribution into that commit.
//! - A conflicted cherry-pick finished with a plain `git commit` (instead of
//!   `git cherry-pick --continue`) keeps the source's attribution.
//! - A conflicted cherry-pick persists what it was picking, so `--continue`
//!   still works after the background service restarts.
//! - A rebase onto a branch that renamed the file moves attribution to the new
//!   path.

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
    fs::write(&file_path, "Base line\nAI one\n").unwrap();
    repo.autter(&["checkpoint", "human", "range.txt"]).unwrap();
    repo.autter(&["checkpoint", "mock_ai", "range.txt"])
        .unwrap();
    repo.stage_all_and_commit("AI one").unwrap();
    file.assert_committed_lines(lines!["Base line".unattributed_human(), "AI one".ai()]);

    fs::write(&file_path, "Base line\nAI one\nAI two\n").unwrap();
    repo.autter(&["checkpoint", "human", "range.txt"]).unwrap();
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
    fs::write(&file_path, "Base line\nAI picked line\n").unwrap();
    repo.autter(&["checkpoint", "human", "nocommit.txt"])
        .unwrap();
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

/// Sets up a cherry-pick that stops on a conflict. Returns the repo, the
/// conflicted file's path and the source commit.
fn conflicted_cherry_pick(name: &str) -> (TestRepo, std::path::PathBuf, String) {
    let repo = TestRepo::new();
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
    fs::write(&file_path, "Line 1\nAI version\nLine 3\n").unwrap();
    repo.autter(&["checkpoint", "human", name]).unwrap();
    repo.autter(&["checkpoint", "mock_ai", name]).unwrap();
    repo.stage_all_and_commit("AI change").unwrap();
    file.assert_committed_lines(lines![
        "Line 1".unattributed_human(),
        "AI version".ai(),
        "Line 3".unattributed_human(),
    ]);
    let source = head_sha(&repo);

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

    (repo, file_path, source)
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
    fs::write(&old_path, "Base line 1\nBase line 2\nAI added line\n").unwrap();
    repo.autter(&["checkpoint", "human", "old_name.txt"])
        .unwrap();
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
