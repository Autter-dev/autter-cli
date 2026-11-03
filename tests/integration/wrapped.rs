//! `autter wrapped`: a fixture year of commits with explicit checkpoints
//! and dates, checked through the JSON contract and the `--no-color` card.

use crate::repos::test_repo::TestRepo;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

const YEAR: &str = "2031";

/// The user message in the Claude transcript; its length is the longest
/// prompt of the year.
const CLAUDE_PROMPT: &str = "Please refactor the config loader so it validates every field before \
use and reports all of the errors at once instead of failing on the first one it finds";

fn commit_on(repo: &TestRepo, message: &str, date: &str) -> String {
    let envs = [("GIT_AUTHOR_DATE", date), ("GIT_COMMITTER_DATE", date)];
    repo.stage_all_and_commit_with_env(message, &envs)
        .unwrap_or_else(|e| panic!("commit {message:?} should succeed: {e}"))
        .commit_sha
}

fn write_claude_transcript(path: &Path, session_id: &str) {
    let user = json!({
        "type": "user",
        "sessionId": session_id,
        "uuid": "u-1",
        "timestamp": "2031-07-07T10:00:00.000Z",
        "message": {"role": "user", "content": CLAUDE_PROMPT},
    });
    let assistant = json!({
        "type": "assistant",
        "sessionId": session_id,
        "uuid": "u-2",
        "timestamp": "2031-07-07T10:00:05.000Z",
        "message": {
            "role": "assistant",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "On it."}],
        },
    });
    fs::write(path, format!("{user}\n{assistant}\n")).unwrap();
}

/// Four commits by the default author across January, March and July of
/// 2031 plus one July commit by someone else. Returns the SHA of the Claude
/// commit, which carries the only locally cached transcript.
///
/// Expected for the default author: 14 added lines, 9 AI (64.29%), 5 known
/// human; Monday is the busiest weekday (Mar 10 and Jul 7) and Mar 10-11 is
/// the longest streak.
fn build_fixture(repo: &mut TestRepo) -> String {
    repo.patch_autter_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
        patch.prompt_storage = Some("local".to_string());
    });
    let root = repo.canonical_path();
    fs::create_dir_all(root.join("src")).unwrap();
    let app = root.join("src").join("app.rs");
    let lib = root.join("src").join("lib.rs");

    // January: two known-human lines.
    fs::write(&app, "fn main() {}\nfn helper() {}\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "src/app.rs"])
        .unwrap();
    commit_on(repo, "Scaffold", "2031-01-05T09:00:00Z");

    // March 10 (Monday): four AI lines from one mock session.
    fs::write(
        &app,
        "fn main() {}\nfn helper() {}\nfn ai_one() {}\nfn ai_two() {}\nfn ai_three() {}\nfn ai_four() {}\n",
    )
    .unwrap();
    repo.autter(&["checkpoint", "mock_ai", "src/app.rs"])
        .unwrap();
    commit_on(repo, "Add handlers", "2031-03-10T09:00:00Z");

    // March 11 (Tuesday): three human lines in a new file, two AI lines in app.rs.
    fs::write(&lib, "pub mod a;\npub mod b;\npub mod c;\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "src/lib.rs"])
        .unwrap();
    fs::write(
        &app,
        "fn main() {}\nfn helper() {}\nfn ai_one() {}\nfn ai_two() {}\nfn ai_three() {}\nfn ai_four() {}\nfn ai_five() {}\nfn ai_six() {}\n",
    )
    .unwrap();
    repo.autter(&["checkpoint", "mock_ai", "src/app.rs"])
        .unwrap();
    commit_on(repo, "Add modules", "2031-03-11T09:00:00Z");

    // July 7 (Monday): three AI lines through the Claude preset with a transcript.
    let session_id = "wrapped-claude-session";
    // Outside the working tree so `git add -A` never commits it.
    let transcript = repo.test_home_path().join("claude-transcript.jsonl");
    write_claude_transcript(&transcript, session_id);
    let hook = |event: &str| {
        json!({
            "cwd": root.to_string_lossy(),
            "hook_event_name": event,
            "session_id": session_id,
            "tool_name": "Edit",
            "tool_use_id": "edit-1",
            "tool_input": {"file_path": app.to_string_lossy()},
            "transcript_path": transcript.to_string_lossy(),
        })
        .to_string()
    };
    repo.autter(&["checkpoint", "claude", "--hook-input", &hook("PreToolUse")])
        .unwrap();
    fs::write(
        &app,
        "fn main() {}\nfn helper() {}\nfn ai_one() {}\nfn ai_two() {}\nfn ai_three() {}\nfn ai_four() {}\nfn ai_five() {}\nfn ai_six() {}\nfn validate() {}\nfn report() {}\nfn collect() {}\n",
    )
    .unwrap();
    repo.autter(&["checkpoint", "claude", "--hook-input", &hook("PostToolUse")])
        .unwrap();
    let claude_commit = commit_on(repo, "Validate config", "2031-07-07T09:00:00Z");

    // July 15: ten AI lines by another author, outside the default scope.
    let other = root.join("src").join("other.rs");
    fs::write(&other, "o1\no2\no3\no4\no5\no6\no7\no8\no9\no10\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "src/other.rs"])
        .unwrap();
    repo.stage_all_and_commit_with_env(
        "Other author",
        &[
            ("GIT_AUTHOR_DATE", "2031-07-15T09:00:00Z"),
            ("GIT_COMMITTER_DATE", "2031-07-15T09:00:00Z"),
            ("GIT_AUTHOR_NAME", "Other Person"),
            ("GIT_AUTHOR_EMAIL", "other@example.com"),
        ],
    )
    .unwrap();

    claude_commit
}

fn wrapped_json(repo: &TestRepo, extra: &[&str]) -> Value {
    let mut args = vec!["wrapped", "--json", "--year", YEAR];
    args.extend_from_slice(extra);
    let output = repo
        .autter_command(&args)
        .output()
        .expect("autter wrapped should run");
    assert!(
        output.status.success(),
        "autter wrapped {extra:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "wrapped --json should print JSON: {e}\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn wrapped_card(repo: &TestRepo, extra: &[&str]) -> String {
    let mut args = vec!["wrapped", "--year", YEAR, "--no-color"];
    args.extend_from_slice(extra);
    let output = repo
        .autter_command(&args)
        // Decide the charset explicitly so the card draws the same on
        // every CI runner.
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .env_remove("TERM")
        .output()
        .expect("autter wrapped should run");
    assert!(
        output.status.success(),
        "autter wrapped {extra:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("card is UTF-8")
}

fn month(summary: &Value, month: u64) -> &Value {
    summary["by_month"]
        .as_array()
        .expect("by_month is an array")
        .iter()
        .find(|entry| entry["month"] == month)
        .unwrap_or_else(|| panic!("month {month} missing"))
}

#[test]
fn wrapped_json_matches_the_share_contract() {
    let mut repo = TestRepo::new();
    let claude_commit = build_fixture(&mut repo);

    let summary = wrapped_json(&repo, &[]);

    assert_eq!(summary["schema_version"], "wrapped/1.0.0");
    assert_eq!(summary["year"], 2031);
    assert_eq!(summary["author"], "test@example.com");
    assert_eq!(summary["all_authors"], false);
    assert_eq!(summary["commits"], 4);
    assert_eq!(summary["commits_with_authorship"], 4);
    assert_eq!(summary["capped"], false);
    assert_eq!(summary["added_lines"], 14);
    assert_eq!(summary["ai_lines"], 9);
    assert_eq!(summary["human_lines"], 5);
    assert_eq!(summary["untracked_lines"], 0);
    assert_eq!(summary["ai_percent"], 64.29);

    assert_eq!(summary["by_month"].as_array().unwrap().len(), 12);
    assert_eq!(
        month(&summary, 1),
        &json!({"month": 1, "ai_accepted": 0, "added_lines": 2, "ai_percent": 0.0, "commits": 1})
    );
    assert_eq!(
        month(&summary, 3),
        &json!({"month": 3, "ai_accepted": 6, "added_lines": 9, "ai_percent": 66.67, "commits": 2})
    );
    assert_eq!(
        month(&summary, 7),
        &json!({"month": 7, "ai_accepted": 3, "added_lines": 3, "ai_percent": 100.0, "commits": 1})
    );
    for empty in [2, 4, 5, 6, 8, 9, 10, 11, 12] {
        assert_eq!(month(&summary, empty)["commits"], 0, "month {empty}");
        assert_eq!(month(&summary, empty)["added_lines"], 0, "month {empty}");
    }

    assert_eq!(summary["top_agent"], json!({"name": "mock_ai", "lines": 6}));
    assert_eq!(
        summary["top_model"],
        json!({"name": "mock_ai/unknown-model", "lines": 6})
    );
    assert_eq!(
        summary["agents"],
        json!([{"name": "mock_ai", "lines": 6}, {"name": "claude", "lines": 3}])
    );
    assert_eq!(
        summary["models"],
        json!([
            {"name": "mock_ai/unknown-model", "lines": 6},
            {"name": "claude-sonnet-4-20250514", "lines": 3}
        ])
    );

    assert_eq!(
        summary["most_prompted_file"],
        json!({"path": "src/app.rs", "prompts": 3})
    );
    assert_eq!(
        summary["files"],
        json!([{"path": "src/app.rs", "prompts": 3}]),
        "known-human-only files are not prompted files"
    );

    assert_eq!(summary["prompts_total"], 3);
    assert_eq!(summary["prompts_with_transcript"], 1);
    let longest = &summary["longest_prompt"];
    assert_eq!(longest["chars"], CLAUDE_PROMPT.chars().count() as u64);
    assert_eq!(
        longest["words"],
        CLAUDE_PROMPT.split_whitespace().count() as u64
    );
    assert_eq!(longest["agent"], "claude");
    assert_eq!(longest["model"], "claude-sonnet-4-20250514");
    assert_eq!(longest["commit"], claude_commit);
    assert_eq!(
        longest["snippet"],
        Value::Null,
        "prompt text stays out of the JSON unless opted in"
    );

    assert_eq!(summary["busiest_day"], "Monday");
    assert_eq!(summary["streak_days"], 2);
}

#[test]
fn wrapped_snippet_is_opt_in() {
    let mut repo = TestRepo::new();
    build_fixture(&mut repo);

    let summary = wrapped_json(&repo, &["--include-prompt-text"]);
    let expected: String = CLAUDE_PROMPT.chars().take(120).collect();
    assert_eq!(summary["longest_prompt"]["snippet"], expected);
    assert_eq!(expected.chars().count(), 120);
}

#[test]
fn wrapped_scopes_to_the_author() {
    let mut repo = TestRepo::new();
    build_fixture(&mut repo);

    let everyone = wrapped_json(&repo, &["--all-authors"]);
    assert_eq!(everyone["author"], Value::Null);
    assert_eq!(everyone["all_authors"], true);
    assert_eq!(everyone["commits"], 5);
    assert_eq!(everyone["added_lines"], 24);
    assert_eq!(everyone["ai_lines"], 19);
    assert_eq!(everyone["ai_percent"], 79.17);
    assert_eq!(
        everyone["top_agent"],
        json!({"name": "mock_ai", "lines": 16})
    );
    assert_eq!(month(&everyone, 7)["commits"], 2);

    let other = wrapped_json(&repo, &["--author", "Other@Example.com"]);
    assert_eq!(other["author"], "Other@Example.com");
    assert_eq!(other["commits"], 1);
    assert_eq!(other["ai_lines"], 10);
    assert_eq!(other["commits_with_authorship"], 1);
    assert_eq!(
        other["most_prompted_file"],
        json!({"path": "src/other.rs", "prompts": 1})
    );
}

#[test]
fn wrapped_other_years_are_empty() {
    let mut repo = TestRepo::new();
    build_fixture(&mut repo);

    let output = repo
        .autter_command(&["wrapped", "--json", "--year", "2030"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["commits"], 0);
    assert_eq!(summary["ai_percent"], 0.0);
    assert_eq!(summary["top_agent"], Value::Null);
    assert_eq!(summary["longest_prompt"], Value::Null);
    assert_eq!(summary["busiest_day"], Value::Null);
    assert_eq!(summary["streak_days"], 0);
}

#[test]
fn wrapped_card_snapshot() {
    let mut repo = TestRepo::new();
    build_fixture(&mut repo);

    let card = wrapped_card(&repo, &[]);
    assert!(card.contains("autter.dev/wrapped"), "{card}");
    insta::assert_snapshot!(card);
}

#[test]
fn wrapped_card_with_prompt_text_shows_a_snippet() {
    let mut repo = TestRepo::new();
    build_fixture(&mut repo);

    let card = wrapped_card(&repo, &["--include-prompt-text"]);
    assert!(
        card.contains("\u{201C}Please refactor the config loader"),
        "{card}"
    );
    let plain = wrapped_card(&repo, &[]);
    assert!(!plain.contains("Please refactor"), "{plain}");
}

#[test]
fn wrapped_empty_state_below_three_noted_commits() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("README.md"), "# Hello\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "README.md"])
        .unwrap();
    commit_on(&repo, "Initial", "2031-02-02T09:00:00Z");

    let card = wrapped_card(&repo, &[]);
    assert_eq!(
        card,
        "◇  Autter Wrapped 2031\n\
         │  1 commit with an authorship note in 2031. A card needs at least 3.\n\
         │  Commit with an autter-tracked agent, then run autter wrapped again.\n\
         └  autter.dev/wrapped\n"
    );

    // The JSON contract is still produced for the share pipeline.
    let summary = wrapped_json(&repo, &[]);
    assert_eq!(summary["commits"], 1);
    assert_eq!(summary["commits_with_authorship"], 1);
}

#[test]
fn wrapped_rejects_bad_arguments() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("README.md"), "# Hello\n").unwrap();
    repo.stage_all_and_commit("Initial").unwrap();

    for args in [
        vec!["wrapped", "--year", "20x"],
        vec!["wrapped", "--year"],
        vec!["wrapped", "--author", "a@b.c", "--all-authors"],
        vec!["wrapped", "--bogus"],
    ] {
        let output = repo.autter_command(&args).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

// --- Sharing -----------------------------------------------------------------

/// A signed-in account in the test home: the file credential store holds a
/// long-lived access token, so `ApiContext::new` sends it as a Bearer token.
fn sign_in(repo: &TestRepo) {
    let path = repo
        .test_home_path()
        .join(".autter")
        .join("internal")
        .join("credentials");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let far_future = 4_102_444_800i64; // 2100-01-01
    fs::write(
        &path,
        json!({
            "access_token": "test-access-token",
            "refresh_token": "test-refresh-token",
            "access_token_expires_at": far_future,
            "refresh_token_expires_at": far_future,
        })
        .to_string(),
    )
    .unwrap();
}

fn run_against(repo: &TestRepo, server: &mockito::Server, args: &[&str]) -> std::process::Output {
    let mut full = vec!["wrapped", "--year", YEAR, "--no-color"];
    full.extend_from_slice(args);
    repo.autter_command(&full)
        .env("AUTTER_API_BASE_URL", server.url())
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .expect("autter wrapped should run")
}

#[test]
fn wrapped_share_flags_are_validated() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("README.md"), "# Hello\n").unwrap();
    repo.stage_all_and_commit("Initial").unwrap();

    for args in [
        vec!["wrapped", "--hide-repo"],
        vec!["wrapped", "--unshare"],
        vec!["wrapped", "--unshare", "https://app.autter.dev/me"],
        vec!["wrapped", "--unshare", "not a token"],
        vec!["wrapped", "--share", "--unshare", "tok1"],
    ] {
        let output = repo.autter_command(&args).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn wrapped_share_sends_nothing_in_local_mode() {
    let mut repo = TestRepo::new();
    build_fixture(&mut repo); // prompt_storage = local
    sign_in(&repo);
    let mut server = mockito::Server::new();
    let post = server
        .mock("POST", "/api/cli/wrapped-share")
        .expect(0)
        .create();
    let delete = server
        .mock("DELETE", mockito::Matcher::Any)
        .expect(0)
        .create();

    let output = run_against(&repo, &server, &["--share"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("local mode"), "{stderr}");

    let output = run_against(&repo, &server, &["--unshare", "tok1"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("local mode"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    post.assert();
    delete.assert();
}

#[test]
fn wrapped_share_without_sign_in_sends_nothing() {
    let mut repo = TestRepo::new();
    build_fixture(&mut repo);
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("default".to_string()));
    let mut server = mockito::Server::new();
    let post = server
        .mock("POST", "/api/cli/wrapped-share")
        .expect(0)
        .create();

    let output = run_against(&repo, &server, &["--share"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("autter login"), "{stderr}");
    post.assert();
}

#[test]
fn wrapped_share_posts_the_summary_and_prints_the_link() {
    let mut repo = TestRepo::new();
    build_fixture(&mut repo);
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("default".to_string()));
    sign_in(&repo);
    let mut server = mockito::Server::new();
    let post = server
        .mock("POST", "/api/cli/wrapped-share")
        .match_header("authorization", "Bearer test-access-token")
        .match_body(mockito::Matcher::PartialJson(json!({
            "hide_repo": true,
            "include_prompt_text": false,
            "wrapped": {
                "schema_version": "wrapped/1.0.0",
                "year": 2031,
                "commits": 4,
                "ai_percent": 64.29,
                "longest_prompt": {"snippet": null}
            }
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"token":"tok1","url":"https://app.autter.dev/wrapped/tok1","image_url":"https://app.autter.dev/public/wrapped/tok1/card.png"}"#)
        .expect(2)
        .create();

    let output = run_against(&repo, &server, &["--share", "--hide-repo"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("autter.dev/wrapped"),
        "card first: {stdout}"
    );
    assert!(
        stdout
            .trim_end()
            .ends_with("Shared at https://app.autter.dev/wrapped/tok1"),
        "{stdout}"
    );

    // With --json, stdout stays the exact contract and the link goes to stderr.
    let output = run_against(&repo, &server, &["--share", "--hide-repo", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let summary: Value =
        serde_json::from_slice(&output.stdout).expect("stdout is the JSON contract");
    assert_eq!(summary["year"], 2031);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("https://app.autter.dev/wrapped/tok1"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    post.assert();
}

#[test]
fn wrapped_share_reports_server_errors_in_one_line() {
    let mut repo = TestRepo::new();
    build_fixture(&mut repo);
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("default".to_string()));
    sign_in(&repo);
    let mut server = mockito::Server::new();
    let post = server
        .mock("POST", "/api/cli/wrapped-share")
        .with_status(403)
        .with_body(r#"{"error":"forbidden"}"#)
        .create();

    let output = run_against(&repo, &server, &["--share"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not a member"), "{stderr}");
    post.assert();
}

#[test]
fn wrapped_unshare_deletes_by_link_or_token() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("README.md"), "# Hello\n").unwrap();
    repo.stage_all_and_commit("Initial").unwrap();
    let mut repo = repo;
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("default".to_string()));
    sign_in(&repo);
    let mut server = mockito::Server::new();
    let ok = server
        .mock("DELETE", "/api/cli/wrapped-share/tok1")
        .match_header("authorization", "Bearer test-access-token")
        .with_status(204)
        .expect(2)
        .create();
    let gone = server
        .mock("DELETE", "/api/cli/wrapped-share/tok2")
        .with_status(404)
        .with_body(r#"{"error":"not found"}"#)
        .create();

    for arg in ["https://app.autter.dev/wrapped/tok1", "tok1"] {
        let output = run_against(&repo, &server, &["--unshare", arg]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Removed"));
    }
    let output = run_against(&repo, &server, &["--unshare", "tok2"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already removed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    ok.assert();
    gone.assert();
}
