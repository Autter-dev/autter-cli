//! Tests for `autter blame --why <file>:<line>`, which explains a single line by
//! resolving the prompt that produced it.

use crate::repos::test_repo::TestRepo;
use std::fs;

/// Build a repo with line 1 = known-human and line 2 = AI, committed.
fn repo_with_human_and_ai_line() -> TestRepo {
    let repo = TestRepo::new();
    let file_path = repo.path().join("f.txt");

    fs::write(&file_path, "human line 1\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "f.txt"])
        .unwrap();

    fs::write(&file_path, "human line 1\nAI made this\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "f.txt"]).unwrap();

    repo.stage_all_and_commit("init").unwrap();
    repo
}

#[test]
fn why_explains_ai_line() {
    let repo = repo_with_human_and_ai_line();

    let output = repo.autter(&["blame", "--why", "f.txt:2"]).unwrap();

    assert!(
        output.contains("f.txt:2"),
        "should echo the target location, got: {output}"
    );
    assert!(
        output.contains("AI made this"),
        "should show the line content, got: {output}"
    );
    assert!(
        output.contains("Produced by AI"),
        "should identify the line as AI-produced, got: {output}"
    );
    assert!(
        output.contains("mock_ai"),
        "should name the agent, got: {output}"
    );
}

#[test]
fn why_reports_human_line_has_no_prompt() {
    let repo = repo_with_human_and_ai_line();

    let output = repo.autter(&["blame", "--why", "f.txt:1"]).unwrap();

    assert!(
        output.contains("f.txt:1") && output.contains("human line 1"),
        "should echo location and content, got: {output}"
    );
    assert!(
        output.contains("written by a human"),
        "human line should report no AI prompt, got: {output}"
    );
    assert!(
        !output.contains("Produced by AI"),
        "human line must not be reported as AI, got: {output}"
    );
}

#[test]
fn why_rejects_out_of_range_line() {
    let repo = repo_with_human_and_ai_line();

    let result = repo.autter(&["blame", "--why", "f.txt:99"]);

    assert!(
        result.is_err(),
        "an out-of-range line should fail, got: {result:?}"
    );
}

#[test]
fn why_rejects_malformed_argument() {
    let repo = repo_with_human_and_ai_line();

    // Missing the `:line` portion.
    assert!(
        repo.autter(&["blame", "--why", "f.txt"]).is_err(),
        "argument without a line number should fail"
    );

    // Non-numeric line.
    assert!(
        repo.autter(&["blame", "--why", "f.txt:abc"]).is_err(),
        "argument with a non-numeric line should fail"
    );
}
