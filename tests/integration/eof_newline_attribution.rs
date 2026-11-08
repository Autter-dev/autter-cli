//! Appending below a last line that has no trailing newline gives that line a `\n`.
//! The line itself is unchanged, so it must keep its author: the append must not
//! make blame or stats credit it to whoever appended.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

#[test]
fn test_ai_append_below_human_line_without_trailing_newline() {
    let repo = TestRepo::new();
    let path = repo.path().join("notes.txt");

    fs::write(&path, "human line").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "notes.txt"])
        .unwrap();
    repo.stage_all_and_commit("human commit").unwrap();
    let mut file = repo.filename("notes.txt");
    file.assert_committed_lines(crate::lines!["human line".human()]);

    // Agent pre-edit and post-edit checkpoints around an append.
    repo.autter(&["checkpoint", "human", "notes.txt"]).unwrap();
    fs::write(&path, "human line\nai line").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "notes.txt"])
        .unwrap();
    let commit = repo.stage_all_and_commit("ai commit").unwrap();

    file.assert_committed_lines(crate::lines!["human line".human(), "ai line".ai()]);
    let stats = repo.stats().unwrap();
    assert_eq!(
        stats.ai_additions, 1,
        "only the appended line is AI: {stats:?}"
    );
    assert_eq!(stats.human_additions, 1, "{stats:?}");
    assert!(
        commit.authorship_log.attestations.iter().all(|file| file
            .entries
            .iter()
            .filter(|entry| !entry.hash.starts_with("h_"))
            .all(|entry| entry.line_ranges.iter().all(|range| !range.contains(1)))),
        "the AI session must not claim line 1: {:?}",
        commit.authorship_log.attestations
    );
}

#[test]
fn test_ai_append_with_trailing_newline_below_unterminated_human_line() {
    let repo = TestRepo::new();
    let path = repo.path().join("notes.txt");

    fs::write(&path, "human line").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "notes.txt"])
        .unwrap();
    repo.stage_all_and_commit("human commit").unwrap();
    let mut file = repo.filename("notes.txt");
    file.assert_committed_lines(crate::lines!["human line".human()]);

    repo.autter(&["checkpoint", "human", "notes.txt"]).unwrap();
    fs::write(&path, "human line\nai line one\nai line two\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "notes.txt"])
        .unwrap();
    repo.stage_all_and_commit("ai commit").unwrap();

    file.assert_committed_lines(crate::lines![
        "human line".human(),
        "ai line one".ai(),
        "ai line two".ai(),
    ]);
}

#[test]
fn test_human_append_below_ai_line_without_trailing_newline() {
    let repo = TestRepo::new();
    let path = repo.path().join("notes.txt");

    repo.autter(&["checkpoint", "human", "notes.txt"]).unwrap();
    fs::write(&path, "first\nai line").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "notes.txt"])
        .unwrap();
    repo.stage_all_and_commit("ai commit").unwrap();
    let mut file = repo.filename("notes.txt");
    file.assert_committed_lines(crate::lines!["first".ai(), "ai line".ai()]);

    fs::write(&path, "first\nai line\nhuman line").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "notes.txt"])
        .unwrap();
    repo.stage_all_and_commit("human commit").unwrap();

    file.assert_committed_lines(crate::lines![
        "first".ai(),
        "ai line".ai(),
        "human line".human(),
    ]);
}

#[test]
fn test_untracked_line_stays_untracked_after_ai_append() {
    let repo = TestRepo::new();
    let path = repo.path().join("notes.txt");

    fs::write(&path, "untracked line").unwrap();
    repo.stage_all_and_commit("untracked commit").unwrap();
    let mut file = repo.filename("notes.txt");
    file.assert_committed_lines(crate::lines!["untracked line".unattributed_human()]);

    repo.autter(&["checkpoint", "human", "notes.txt"]).unwrap();
    fs::write(&path, "untracked line\nai line").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "notes.txt"])
        .unwrap();
    repo.stage_all_and_commit("ai commit").unwrap();

    file.assert_committed_lines(crate::lines![
        "untracked line".unattributed_human(),
        "ai line".ai(),
    ]);
}
