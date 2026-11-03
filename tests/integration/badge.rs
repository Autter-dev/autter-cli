//! `autter badge`: the repo-wide AI share as a shields.io endpoint payload
//! and a README snippet.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use serde_json::Value;
use std::fs;

/// Run `autter badge --json <args>` and parse the payload line.
fn badge_json(repo: &TestRepo, args: &[&str]) -> Value {
    let mut full = vec!["badge", "--json"];
    full.extend_from_slice(args);
    let output = repo.autter(&full).expect("autter badge --json succeeds");
    let line = output
        .lines()
        .find(|line| line.starts_with('{') && line.contains("\"schemaVersion\""))
        .unwrap_or_else(|| panic!("no badge payload in output:\n{output}"));
    serde_json::from_str(line).expect("badge payload is JSON")
}

/// Stdout of `autter badge <args>` alone, without the debug lines on stderr.
fn badge_stdout(repo: &TestRepo, args: &[&str]) -> String {
    let mut full = vec!["badge"];
    full.extend_from_slice(args);
    let output = repo
        .autter_command(&full)
        .output()
        .expect("autter badge runs");
    assert!(
        output.status.success(),
        "autter badge failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn assert_shields_keys(payload: &Value) {
    assert_eq!(payload["schemaVersion"], 1);
    assert_eq!(payload["label"], "AI-written");
    assert!(payload["message"].is_string());
    assert!(payload["color"].is_string());
}

#[test]
fn badge_json_all_ai_repo() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("app.txt");

    fs::write(&file_path, "AI line 1\nAI line 2\nAI line 3\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.stage_all_and_commit("All AI").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["AI line 1".ai(), "AI line 2".ai(), "AI line 3".ai(),]);

    let payload = badge_json(&repo, &[]);
    assert_shields_keys(&payload);
    assert_eq!(payload["message"], "100%");
    assert_eq!(payload["color"], "2ee6c5");
    assert_eq!(payload["ai_percent"], 100.0);
    assert_eq!(payload["ai_accepted"], 3);
    assert_eq!(payload["added_lines"], 3);
    assert_eq!(payload["commits"], 1);
    assert_eq!(payload["commits_with_authorship"], 1);
    assert_eq!(payload["capped"], false);
    // The default window is dated: an ISO date 90 days back.
    let since = payload["since"]
        .as_str()
        .expect("default window has a since date");
    assert!(
        chrono::NaiveDate::parse_from_str(since, "%Y-%m-%d").is_ok(),
        "since should be an ISO date, got {since}"
    );
}

#[test]
fn badge_json_mixed_repo() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("app.txt");

    // Commit 1: one known-human line, then one AI line.
    fs::write(&file_path, "Human line 1\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "app.txt"])
        .unwrap();
    fs::write(&file_path, "Human line 1\nAI line 1\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.stage_all_and_commit("Mixed 1").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["Human line 1".human(), "AI line 1".ai()]);

    // Commit 2: two more human lines, then two more AI lines.
    fs::write(
        &file_path,
        "Human line 1\nAI line 1\nHuman line 2\nHuman line 3\n",
    )
    .unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "app.txt"])
        .unwrap();
    fs::write(
        &file_path,
        "Human line 1\nAI line 1\nHuman line 2\nHuman line 3\nAI line 2\nAI line 3\n",
    )
    .unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.stage_all_and_commit("Mixed 2").unwrap();
    file.assert_committed_lines(lines![
        "Human line 1".human(),
        "AI line 1".ai(),
        "Human line 2".human(),
        "Human line 3".human(),
        "AI line 2".ai(),
        "AI line 3".ai(),
    ]);

    // 3 AI-accepted of 6 added lines across both commits.
    let payload = badge_json(&repo, &[]);
    assert_shields_keys(&payload);
    assert_eq!(payload["ai_accepted"], 3);
    assert_eq!(payload["added_lines"], 6);
    assert_eq!(payload["ai_percent"], 50.0);
    assert_eq!(payload["message"], "50%");
    assert_eq!(payload["color"], "5b5bd6");
    assert_eq!(payload["commits"], 2);
    assert_eq!(payload["commits_with_authorship"], 2);
}

#[test]
fn badge_json_reports_coverage_for_commits_without_notes() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("app.txt");

    fs::write(&file_path, "AI line 1\nAI line 2\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.stage_all_and_commit("With note").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["AI line 1".ai(), "AI line 2".ai()]);

    // A commit whose authorship note is gone (made elsewhere, never synced).
    let other_path = repo.path().join("other.txt");
    fs::write(&other_path, "Untracked 1\nUntracked 2\nUntracked 3\n").unwrap();
    let commit = repo.stage_all_and_commit("Without note").unwrap();
    let mut other = repo.filename("other.txt");
    other.assert_committed_lines(lines![
        "Untracked 1".unattributed_human(),
        "Untracked 2".unattributed_human(),
        "Untracked 3".unattributed_human(),
    ]);
    let _ = repo.git_og(&["notes", "--ref=ai", "remove", &commit.commit_sha]);
    assert!(
        repo.read_authorship_note(&commit.commit_sha).is_none(),
        "the second commit must have no authorship note"
    );

    let payload = badge_json(&repo, &[]);
    assert_eq!(payload["commits"], 2);
    assert_eq!(payload["commits_with_authorship"], 1);
    // The note-less commit is counted but adds nothing to the share.
    assert_eq!(payload["ai_accepted"], 2);
    assert_eq!(payload["added_lines"], 2);
    assert_eq!(payload["ai_percent"], 100.0);
}

#[test]
fn badge_json_respects_autter_ignore() {
    let repo = TestRepo::new();
    fs::write(repo.path().join(".autter-ignore"), "*.lock\n").unwrap();
    fs::write(repo.path().join("app.txt"), "AI line 1\nAI line 2\n").unwrap();
    fs::write(
        repo.path().join("deps.lock"),
        "lock 1\nlock 2\nlock 3\nlock 4\nlock 5\n",
    )
    .unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt", "deps.lock"])
        .unwrap();
    repo.stage_all_and_commit("With lock file").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["AI line 1".ai(), "AI line 2".ai()]);

    let payload = badge_json(&repo, &[]);
    assert_eq!(payload["ai_accepted"], 2);
    // app.txt plus the one-line .autter-ignore; deps.lock is left out.
    assert_eq!(payload["added_lines"], 3);
    assert_eq!(payload["ai_percent"], 66.67);
    assert_eq!(payload["message"], "67%");

    // `--ignore` adds to the patterns from the file.
    let payload = badge_json(&repo, &["--ignore", ".autter-ignore"]);
    assert_eq!(payload["added_lines"], 2);
    assert_eq!(payload["ai_percent"], 100.0);
}

#[test]
fn badge_json_window_flags() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("app.txt"), "AI line 1\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.stage_all_and_commit("One").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["AI line 1".ai()]);

    let payload = badge_json(&repo, &["--all-time"]);
    assert_eq!(payload["since"], Value::Null);
    assert_eq!(payload["commits"], 1);
    assert_eq!(payload["message"], "100%");

    let payload = badge_json(&repo, &["--since", "1 day ago"]);
    assert_eq!(payload["since"], "1 day ago");
    assert_eq!(payload["commits"], 1);

    // Nothing in the window: no data, grey badge, zero share. (git clamps a
    // far-future year such as 2999 to the current year, so use a relative date.)
    let payload = badge_json(&repo, &["--since=tomorrow"]);
    assert_shields_keys(&payload);
    assert_eq!(payload["commits"], 0);
    assert_eq!(payload["commits_with_authorship"], 0);
    assert_eq!(payload["added_lines"], 0);
    assert_eq!(payload["ai_percent"], 0.0);
    assert_eq!(payload["message"], "no data");
    assert_eq!(payload["color"], "lightgrey");

    let err = repo
        .autter(&["badge", "--since", "1 day ago", "--all-time"])
        .unwrap_err();
    assert!(err.contains("only one of --since and --all-time"), "{err}");
}

#[test]
fn badge_write_stores_the_payload() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("app.txt"), "AI line 1\nAI line 2\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.stage_all_and_commit("One").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["AI line 1".ai(), "AI line 2".ai()]);

    // Default path.
    let printed = badge_json(&repo, &["--write"]);
    let written: Value = serde_json::from_str(
        &fs::read_to_string(repo.path().join(".autter/badge.json")).expect("badge.json written"),
    )
    .expect("badge.json is JSON");
    assert_eq!(written, printed);
    assert_shields_keys(&written);
    assert_eq!(written["message"], "100%");

    // Custom path, created with its parent directory.
    let printed = badge_json(&repo, &["--all-time", "--write", "docs/ai-badge.json"]);
    let written: Value = serde_json::from_str(
        &fs::read_to_string(repo.path().join("docs/ai-badge.json")).expect("custom path written"),
    )
    .unwrap();
    assert_eq!(written, printed);
    assert_eq!(written["since"], Value::Null);
}

#[test]
fn badge_markdown_snippet_without_cloud() {
    let repo = TestRepo::new();
    fs::write(
        repo.path().join("app.txt"),
        "Human line\nAI line 1\nAI line 2\nAI line 3\n",
    )
    .unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "app.txt"])
        .unwrap();
    repo.stage_all_and_commit("Human").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines![
        "Human line".human(),
        "AI line 1".human(),
        "AI line 2".human(),
        "AI line 3".human(),
    ]);

    fs::write(
        repo.path().join("app.txt"),
        "Human line\nAI line 1\nAI line 2\nAI line 3\nAI line 4\nAI line 5\nAI line 6\nAI line 7\nAI line 8\nAI line 9\nAI line 10\nAI line 11\nAI line 12\n",
    )
    .unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.stage_all_and_commit("AI").unwrap();
    file.assert_committed_lines(lines![
        "Human line".human(),
        "AI line 1".human(),
        "AI line 2".human(),
        "AI line 3".human(),
        "AI line 4".ai(),
        "AI line 5".ai(),
        "AI line 6".ai(),
        "AI line 7".ai(),
        "AI line 8".ai(),
        "AI line 9".ai(),
        "AI line 10".ai(),
        "AI line 11".ai(),
        "AI line 12".ai(),
    ]);

    // 9 of 13 added lines: 69.23%, shown as 69%, in the 50-74% color.
    let output = badge_stdout(&repo, &["--all-time"]);
    insta::assert_snapshot!("badge_markdown_snippet_without_cloud", output);
}

#[test]
fn badge_write_with_github_remote_prints_endpoint_snippet() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("app.txt"), "AI line 1\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.stage_all_and_commit("One").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(lines!["AI line 1".ai()]);
    repo.git_og(&["remote", "add", "origin", "git@github.com:acme/widgets.git"])
        .unwrap();
    let branch = repo.current_branch();

    let output = badge_stdout(&repo, &["--all-time", "--write"]);
    let expected = format!(
        "[![AI-written](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Facme%2Fwidgets%2F{branch}%2F.autter%2Fbadge.json)](https://autter.dev)"
    );
    assert!(output.contains(&expected), "unexpected output:\n{output}");
    assert!(
        output.contains(
            "Static badge: AI-written reads .autter/badge.json once it is committed and pushed"
        ),
        "unexpected output:\n{output}"
    );
    assert!(repo.path().join(".autter/badge.json").is_file());
}

#[test]
fn badge_help_lists_flags_and_colors() {
    let repo = TestRepo::new();
    let output = repo.autter(&["badge", "--help"]).unwrap();
    for needle in [
        "--since",
        "--all-time",
        "--write",
        "--json",
        "lightgrey",
        "2ee6c5",
    ] {
        assert!(
            output.contains(needle),
            "help should mention {needle}:\n{output}"
        );
    }
    let overview = repo.autter(&["help"]).unwrap();
    assert!(overview.contains("badge"), "{overview}");
}
