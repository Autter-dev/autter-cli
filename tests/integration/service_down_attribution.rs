//! Commits made while the background service couldn't see them (stopped,
//! crashed, still starting, hung, or git's trace2 config missing) used to lose
//! their attribution silently: the commit succeeded, the AI checkpoints stayed
//! on disk under the parent sha, and nothing ever turned them into a note.
//!
//! `git_og` runs plain git with no trace2 and no hooks, which is exactly what
//! the service sees in those cases: nothing.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn head_sha(repo: &TestRepo) -> String {
    repo.git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn test_commit_made_while_service_down_is_attributed_when_it_returns() {
    let repo = TestRepo::new();
    let path = repo.path().join("app.txt");

    fs::write(&path, "base line\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["base line".unattributed_human()]);

    // An agent edit: pre-edit (untracked) checkpoint, then the AI checkpoint.
    repo.autter(&["checkpoint", "human", "app.txt"]).unwrap();
    fs::write(&path, "base line\nai line one\nai line two\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();

    // The commit happens while the service can't see it.
    repo.git_og(&["add", "app.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "made while the service was down"])
        .unwrap();
    let stranded = head_sha(&repo);
    assert!(
        repo.read_authorship_note(&stranded).is_none(),
        "nothing observed the commit, so it has no note yet"
    );

    // The service is back: the next commit it sees also rebuilds the note for
    // the one it missed, from the checkpoints still on disk.
    fs::write(repo.path().join("notes.txt"), "a human note\n").unwrap();
    repo.stage_all_and_commit("service is back").unwrap();

    assert!(
        repo.read_authorship_note(&stranded).is_some(),
        "the missed commit's note is rebuilt"
    );
    file.assert_committed_lines(lines![
        "base line".unattributed_human(),
        "ai line one".ai(),
        "ai line two".ai(),
    ]);
    let mut notes = repo.filename("notes.txt");
    notes.assert_committed_lines(lines!["a human note".unattributed_human()]);
}

#[test]
fn test_several_missed_commits_are_recovered_in_order() {
    let repo = TestRepo::new();
    let path = repo.path().join("lib.txt");

    fs::write(&path, "one\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("lib.txt");
    file.assert_committed_lines(lines!["one".unattributed_human()]);

    // First missed commit: an AI line.
    repo.autter(&["checkpoint", "human", "lib.txt"]).unwrap();
    fs::write(&path, "one\nai two\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "lib.txt"]).unwrap();
    repo.git_og(&["add", "lib.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "missed one"]).unwrap();
    let first = head_sha(&repo);

    // Second missed commit, on top of the first: a known-human line, then an
    // AI line.
    fs::write(&path, "one\nai two\nhuman three\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "lib.txt"])
        .unwrap();
    repo.autter(&["checkpoint", "human", "lib.txt"]).unwrap();
    fs::write(&path, "one\nai two\nhuman three\nai four\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "lib.txt"]).unwrap();
    repo.git_og(&["add", "lib.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "missed two"]).unwrap();
    let second = head_sha(&repo);
    assert!(repo.read_authorship_note(&first).is_none());
    assert!(repo.read_authorship_note(&second).is_none());

    // Service back.
    fs::write(repo.path().join("other.txt"), "later\n").unwrap();
    repo.stage_all_and_commit("service is back").unwrap();

    assert!(repo.read_authorship_note(&first).is_some());
    assert!(repo.read_authorship_note(&second).is_some());
    file.assert_committed_lines(lines![
        "one".unattributed_human(),
        "ai two".ai(),
        "human three".human(),
        "ai four".ai(),
    ]);
    let mut other = repo.filename("other.txt");
    other.assert_committed_lines(lines!["later".unattributed_human()]);
}

#[test]
fn test_someone_elses_commit_does_not_take_local_checkpoints() {
    let repo = TestRepo::new();
    let path = repo.path().join("shared.txt");

    fs::write(&path, "start\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("shared.txt");
    file.assert_committed_lines(lines!["start".unattributed_human()]);

    repo.autter(&["checkpoint", "human", "shared.txt"]).unwrap();
    fs::write(&path, "start\nadded\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "shared.txt"])
        .unwrap();

    // Committed under another author's identity while the service was down
    // (stands in for a commit pulled in from someone else).
    repo.git_og(&["add", "shared.txt"]).unwrap();
    repo.git_og_with_env(
        &["commit", "-m", "someone else"],
        &[
            ("GIT_AUTHOR_NAME", "Someone Else"),
            ("GIT_AUTHOR_EMAIL", "someone.else@example.com"),
        ],
    )
    .unwrap();
    let foreign = head_sha(&repo);

    fs::write(repo.path().join("mine.txt"), "mine\n").unwrap();
    repo.stage_all_and_commit("mine").unwrap();

    assert!(
        repo.read_authorship_note(&foreign).is_none(),
        "another author's commit is never rebuilt from this machine's checkpoints"
    );
    file.assert_committed_lines(lines![
        "start".unattributed_human(),
        "added".unattributed_human(),
    ]);
    let mut mine = repo.filename("mine.txt");
    mine.assert_committed_lines(lines!["mine".unattributed_human()]);
}
