use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use regex::Regex;
use std::fs;

// Local helper mirroring the CLI arg vector used by main
fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

// Reimport the parsing function from the show_prompt command module
use autter::commands::show_prompt::parse_args;

#[test]
fn parse_args_requires_prompt_id() {
    let result = parse_args(&args(&[]));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "show-prompt requires a prompt ID");
}

#[test]
fn parse_args_parses_basic_id() {
    let result = parse_args(&args(&["my-prompt-id"])).unwrap();
    assert_eq!(result.prompt_id, "my-prompt-id");
    assert!(result.commit.is_none());
    assert_eq!(result.offset, 0);
}

#[test]
fn parse_args_parses_commit_flag() {
    let result = parse_args(&args(&["my-id", "--commit", "HEAD"])).unwrap();
    assert_eq!(result.prompt_id, "my-id");
    assert_eq!(result.commit.as_deref(), Some("HEAD"));
    assert_eq!(result.offset, 0);
}

#[test]
fn parse_args_parses_offset_flag() {
    let result = parse_args(&args(&["my-id", "--offset", "2"])).unwrap();
    assert_eq!(result.prompt_id, "my-id");
    assert!(result.commit.is_none());
    assert_eq!(result.offset, 2);
}

#[test]
fn parse_args_rejects_commit_and_offset_together() {
    let result = parse_args(&args(&["id", "--commit", "HEAD", "--offset", "1"]));
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "--commit and --offset are mutually exclusive"
    );
}

#[test]
fn parse_args_rejects_multiple_prompt_ids() {
    let result = parse_args(&args(&["id1", "id2"]));
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "Only one prompt ID can be specified".to_string()
    );
}

#[test]
fn parse_args_requires_commit_value() {
    let result = parse_args(&args(&["id", "--commit"]));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "--commit requires a value");
}

#[test]
fn parse_args_requires_offset_value() {
    let result = parse_args(&args(&["id", "--offset"]));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "--offset requires a value");
}

#[test]
fn parse_args_rejects_invalid_offset() {
    let result = parse_args(&args(&["id", "--offset", "not-a-number"]));
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "--offset must be a non-negative integer"
    );
}

#[test]
fn parse_args_rejects_unknown_flag() {
    let result = parse_args(&args(&["id", "--unknown"]));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "Unknown option: --unknown");
}

#[test]
fn show_prompt_returns_latest_prompt_by_default() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // First AI session
    file.set_contents(crate::lines!["Base".human(), "AI line 1".ai()]);
    repo.stage_all_and_commit("First commit").unwrap();

    // Second AI session with a different AI line so we have distinct prompts
    file.insert_at(2, crate::lines!["AI line 2".ai()]);
    let second_commit = repo.stage_all_and_commit("Second commit").unwrap();

    // Grab one of the session IDs from the latest commit's authorship log
    // (mock_ai now produces sessions instead of prompts)
    let sessions = &second_commit.authorship_log.metadata.sessions;
    let (prompt_id, _session) = sessions
        .iter()
        .next()
        .expect("expected at least one session");

    // show-prompt should return the latest occurrence by default
    let output = repo
        .autter(&["show-prompt", prompt_id])
        .expect("show-prompt should succeed");

    let json: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(json["prompt_id"].as_str(), Some(prompt_id.as_str()));
    assert_eq!(
        json["commit"].as_str(),
        Some(second_commit.commit_sha.as_str())
    );
}

#[test]
fn show_prompt_with_offset_skips_occurrences() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Both commits get the same timestamp: commits made within one second
    // (a rebase, a script) must still be ordered by history, not arbitrarily.
    let same_second = [
        ("GIT_AUTHOR_DATE", "2030-01-01T00:00:00Z"),
        ("GIT_COMMITTER_DATE", "2030-01-01T00:00:00Z"),
    ];

    // First AI session
    file.set_contents(crate::lines!["Base".human(), "AI line 1".ai()]);
    let first_commit = repo
        .stage_all_and_commit_with_env("First commit", &same_second)
        .unwrap();

    // Second AI session appends below "AI line 1", which had no trailing newline.
    // That line gains a "\n", so git reports it as changed in the second commit
    // and its note carries the first session's attribution for it: the first
    // session now occurs in both commits.
    file.insert_at(2, crate::lines!["AI line 2".ai()]);
    let second_commit = repo
        .stage_all_and_commit_with_env("Second commit", &same_second)
        .unwrap();
    file.assert_committed_lines(crate::lines![
        "Base".human(),
        "AI line 1".ai(),
        "AI line 2".ai(),
    ]);

    // Use a session ID from the first commit (mock_ai now produces sessions)
    let sessions_first = &first_commit.authorship_log.metadata.sessions;
    let (prompt_id, _) = sessions_first
        .iter()
        .next()
        .expect("expected at least one session in first commit");
    assert!(
        second_commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(prompt_id),
        "the carried line keeps the first session's attribution"
    );

    let commit_for = |offset: Option<&str>| {
        let mut args = vec!["show-prompt", prompt_id.as_str()];
        if let Some(offset) = offset {
            args.extend(["--offset", offset]);
        }
        let output = repo
            .autter(&args)
            .unwrap_or_else(|e| panic!("show-prompt {args:?} should succeed: {e}"));
        let json: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        json["commit"].as_str().unwrap().to_string()
    };

    // Default (no offset) and explicit offset 0 both point to the latest occurrence;
    // offset 1 skips back to the first commit.
    assert_eq!(commit_for(None), second_commit.commit_sha);
    assert_eq!(commit_for(Some("0")), second_commit.commit_sha);
    assert_eq!(commit_for(Some("1")), first_commit.commit_sha);

    // Offset that is too large should return a clear error
    let err = repo
        .autter(&["show-prompt", prompt_id, "--offset", "2"])
        .expect_err("show-prompt with offset 2 should fail when only two occurrences exist");
    assert!(
        err.contains("found 2 time(s), but offset 2 requested"),
        "unexpected error message: {}",
        err
    );
}

/// One committed AI line (line 2) under a known-human line, so blame has a
/// prompt to point at.
fn repo_with_ai_line() -> TestRepo {
    let repo = TestRepo::new();
    let file_path = repo.path().join("f.txt");
    fs::write(&file_path, "human line 1\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "f.txt"])
        .unwrap();
    fs::write(&file_path, "human line 1\nAI made this\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "f.txt"]).unwrap();
    repo.stage_all_and_commit("init").unwrap();
    let mut file = repo.filename("f.txt");
    file.assert_committed_lines(crate::lines!["human line 1".human(), "AI made this".ai()]);
    repo
}

/// The only session ID in HEAD's authorship note.
fn head_session_id(repo: &TestRepo) -> String {
    let note = repo
        .read_authorship_note(&repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string())
        .expect("HEAD has an authorship note");
    let log =
        autter::authorship::authorship_log_serialization::AuthorshipLog::deserialize_from_string(
            &note,
        )
        .expect("note parses");
    let mut ids: Vec<&String> = log.metadata.sessions.keys().collect();
    assert_eq!(ids.len(), 1, "expected one session, got {ids:?}");
    ids.pop().unwrap().clone()
}

/// Replace IDs, SHAs and dates that change from run to run.
fn redact(text: &str) -> String {
    let sessions = Regex::new(r"s_[0-9a-f]{4,14}").unwrap();
    let shas = Regex::new(r"\b[0-9a-f]{7,40}\b").unwrap();
    let dates = Regex::new(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} [+-]\d{4}").unwrap();
    let text = sessions.replace_all(text, "s_<id>");
    let text = shas.replace_all(&text, "<sha>");
    dates.replace_all(&text, "<date>").to_string()
}

fn json(output: &str) -> serde_json::Value {
    serde_json::from_str(output.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {output}"))
}

#[test]
fn blame_legend_shows_prompt_reference_on_a_terminal() {
    let repo = repo_with_ai_line();
    let session = head_session_id(&repo);
    let short = format!("s_{}", &session[2..10]);

    let output = repo
        .autter_with_env(&["blame", "f.txt"], &[("AUTTER_TEST_FORCE_TTY", "1")])
        .unwrap();
    assert!(
        output.contains(&short),
        "legend should show {short}, got:\n{output}"
    );
    insta::assert_snapshot!(redact(&output));
}

#[test]
fn piped_blame_has_no_legend() {
    let repo = repo_with_ai_line();
    let output = repo.autter(&["blame", "f.txt"]).unwrap();
    assert!(
        !output.contains("Prompts behind the AI lines"),
        "piped blame must stay one line per source line, got:\n{output}"
    );
    assert_eq!(output.lines().count(), 2, "got:\n{output}");
}

#[test]
fn show_prompt_flag_puts_reference_on_ai_lines() {
    let repo = repo_with_ai_line();
    let session = head_session_id(&repo);
    let short = format!("s_{}", &session[2..10]);

    let output = repo.autter(&["blame", "--show-prompt", "f.txt"]).unwrap();
    assert!(
        output.contains(&format!("mock_ai [{short}]")),
        "AI line should carry its reference, got:\n{output}"
    );
    assert!(
        output.contains("Prompts behind the AI lines"),
        "--show-prompt should print the legend even when piped, got:\n{output}"
    );
}

#[test]
fn show_prompt_resolves_the_short_reference_from_blame() {
    let repo = repo_with_ai_line();
    let session = head_session_id(&repo);
    let short = format!("s_{}", &session[2..10]);

    let output = repo.autter(&["show-prompt", &short]).unwrap();
    let value = json(&output);
    assert_eq!(value["prompt_id"].as_str(), Some(session.as_str()));
    assert_eq!(value["query"].as_str(), Some(short.as_str()));
    assert_eq!(
        value["prompt"]["agent_id"]["tool"].as_str(),
        Some("mock_ai")
    );
    assert_eq!(
        value["commit"].as_str(),
        Some(repo.git(&["rev-parse", "HEAD"]).unwrap().trim())
    );
    // mock_ai records no transcript; the output says so instead of omitting it.
    assert_eq!(value["transcript"]["status"].as_str(), Some("not_recorded"));
    assert!(value["transcript"]["reason"].as_str().is_some());

    // The bracketed form `--show-prompt` prints, and the --commit path, work too.
    let bracketed = json(
        &repo
            .autter(&["show-prompt", &format!("[{short}]")])
            .unwrap(),
    );
    assert_eq!(bracketed["prompt_id"].as_str(), Some(session.as_str()));
    let in_commit = json(
        &repo
            .autter(&["show-prompt", &short, "--commit", "HEAD"])
            .unwrap(),
    );
    assert_eq!(in_commit["prompt_id"].as_str(), Some(session.as_str()));
}

#[test]
fn show_prompt_resolves_the_agent_session_id() {
    let repo = repo_with_ai_line();
    let session = head_session_id(&repo);
    let full = json(&repo.autter(&["show-prompt", &session]).unwrap());
    let agent_session = full["prompt"]["agent_id"]["id"]
        .as_str()
        .expect("agent session id")
        .to_string();

    let value = json(&repo.autter(&["show-prompt", &agent_session]).unwrap());
    assert_eq!(value["prompt_id"].as_str(), Some(session.as_str()));
}

#[test]
fn why_reference_resolves_in_show_prompt() {
    let repo = repo_with_ai_line();
    let session = head_session_id(&repo);

    let why = repo.autter(&["blame", "--why", "f.txt:2"]).unwrap();
    let reference = why
        .lines()
        .find_map(|l| l.trim().strip_prefix("prompt:"))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("--why should print a prompt reference:\n{why}"))
        .to_string();
    assert!(
        why.contains(&format!("autter show-prompt {reference}")),
        "{why}"
    );
    assert!(
        why.contains("commit:"),
        "--why should name the commit:\n{why}"
    );
    assert!(
        why.contains("No transcript was saved"),
        "--why should say why there is no transcript:\n{why}"
    );
    insta::assert_snapshot!(redact(&why));

    let value = json(&repo.autter(&["show-prompt", &reference]).unwrap());
    assert_eq!(value["prompt_id"].as_str(), Some(session.as_str()));
}

#[test]
fn show_prompt_help_explains_the_id() {
    let repo = TestRepo::new();
    let long = repo.autter(&["show-prompt", "--help"]).unwrap();
    let short = repo.autter(&["show-prompt", "-h"]).unwrap();
    assert_eq!(long, short);
    assert!(!long.contains("Unknown option"), "{long}");
    insta::assert_snapshot!(long);
}

#[test]
fn show_prompt_unknown_id_says_where_ids_come_from() {
    let repo = repo_with_ai_line();
    let err = repo
        .autter(&["show-prompt", "s_00000000"])
        .expect_err("unknown ID should fail");
    assert!(
        err.contains("Prompt not found in history: s_00000000"),
        "{err}"
    );
    assert!(err.contains("autter blame --why"), "{err}");
    assert!(err.contains("autter fetch-notes"), "{err}");
}

crate::reuse_tests_in_worktree!(
    parse_args_requires_prompt_id,
    parse_args_parses_basic_id,
    parse_args_parses_commit_flag,
    parse_args_parses_offset_flag,
    parse_args_rejects_commit_and_offset_together,
    parse_args_rejects_multiple_prompt_ids,
    parse_args_requires_commit_value,
    parse_args_requires_offset_value,
    parse_args_rejects_invalid_offset,
    parse_args_rejects_unknown_flag,
    show_prompt_returns_latest_prompt_by_default,
    show_prompt_with_offset_skips_occurrences,
    blame_legend_shows_prompt_reference_on_a_terminal,
    piped_blame_has_no_legend,
    show_prompt_flag_puts_reference_on_ai_lines,
    show_prompt_resolves_the_short_reference_from_blame,
    show_prompt_resolves_the_agent_session_id,
    why_reference_resolves_in_show_prompt,
    show_prompt_help_explains_the_id,
    show_prompt_unknown_id_says_where_ids_come_from,
);
