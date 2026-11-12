//! Kiro hooks: edits reported through the documented PreToolUse/PostToolUse
//! envelope (https://kiro.dev/docs/hooks/types/) are attributed to the agent.
//! The `tool_input` field names are UNCONFIRMED until a real capture (see
//! docs/kiro-capture.md); these tests pin the shapes the preset accepts.
//! Installing into ~/.kiro/hooks/autter.json is covered in-process by the
//! installer's unit tests (running `install-hooks` here would drive real
//! editor CLIs found on PATH).

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use serde_json::{Value, json};
use std::fs;

fn envelope(repo: &TestRepo, event: &str, tool: &str, input: Value) -> String {
    json!({
        "hook_event_name": event,
        "cwd": repo.canonical_path().to_string_lossy(),
        "session_id": "kiro-session-1",
        "tool_name": tool,
        "tool_input": input,
    })
    .to_string()
}

/// A Kiro hook must print nothing the agent would read: on exit 0 Kiro adds
/// stdout to the agent's context. The test harness's own counters are the
/// only allowed lines.
fn assert_silent(output: &str) {
    for line in output.lines() {
        assert!(
            line.starts_with("checkpoint_requests=")
                || line.starts_with("session_activity_requests=")
                || line.trim().is_empty(),
            "kiro hook printed {line:?} (full output: {output})"
        );
    }
}

fn hook(repo: &TestRepo, payload: &str) -> String {
    let out = repo
        .autter_with_stdin(
            &["checkpoint", "kiro", "--hook-input", "stdin"],
            payload.as_bytes(),
        )
        .unwrap_or_else(|e| panic!("kiro hook failed: {e}"));
    assert_silent(&out);
    out
}

#[test]
fn kiro_file_write_is_attributed_to_ai() {
    let repo = TestRepo::new();
    let file_path = repo.canonical_path().join("agent.rs");
    fs::write(&file_path, "fn human() {}\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("agent.rs");
    file.assert_committed_lines(crate::lines!["fn human() {}".unattributed_human()]);

    let input = json!({"command": "str_replace", "path": "agent.rs",
                       "old_str": "fn human() {}", "new_str": "fn human() {}\nfn agent() {}"});
    hook(
        &repo,
        &envelope(&repo, "PreToolUse", "fs_write", input.clone()),
    );
    fs::write(&file_path, "fn human() {}\nfn agent() {}\n").unwrap();
    hook(&repo, &envelope(&repo, "PostToolUse", "fs_write", input));

    repo.stage_all_and_commit("agent edit").unwrap();
    file.assert_committed_lines(crate::lines![
        "fn human() {}".unattributed_human(),
        "fn agent() {}".ai(),
    ]);
}

/// The Kiro IDE capture published by AWS Labs shows PostToolUse with an empty
/// `tool_input` and the path only in `tool_response` prose.
#[test]
fn kiro_post_with_the_path_only_in_the_response_is_attributed_to_ai() {
    let repo = TestRepo::new();
    let file_path = repo.canonical_path().join("notes.md");
    fs::write(&file_path, "Human line\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("notes.md");
    file.assert_committed_lines(crate::lines!["Human line".unattributed_human()]);

    hook(
        &repo,
        &envelope(
            &repo,
            "PreToolUse",
            "fs_write",
            json!({"path": "notes.md", "text": "AI line"}),
        ),
    );
    fs::write(&file_path, "Human line\nAI line\n").unwrap();
    let post = json!({
        "hook_event_name": "PostToolUse",
        "cwd": repo.canonical_path().to_string_lossy(),
        "session_id": "kiro-session-1",
        "tool_name": "fs_append",
        "tool_input": {},
        "tool_response": "Appended the text to the notes.md file.",
    })
    .to_string();
    hook(&repo, &post);

    repo.stage_all_and_commit("agent append").unwrap();
    file.assert_committed_lines(crate::lines![
        "Human line".unattributed_human(),
        "AI line".ai(),
    ]);
}

#[test]
fn kiro_shell_command_edits_are_attributed_to_ai() {
    let repo = TestRepo::new();
    let file_path = repo.canonical_path().join("gen.txt");
    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("gen.txt");
    file.assert_committed_lines(crate::lines!["base".unattributed_human()]);

    let input = json!({"command": "echo generated >> gen.txt"});
    hook(
        &repo,
        &envelope(&repo, "PreToolUse", "execute_bash", input.clone()),
    );
    fs::write(&file_path, "base\ngenerated\n").unwrap();
    hook(
        &repo,
        &envelope(&repo, "PostToolUse", "execute_bash", input),
    );

    repo.stage_all_and_commit("agent shell edit").unwrap();
    file.assert_committed_lines(crate::lines!["base".unattributed_human(), "generated".ai(),]);
}

/// Reads, prompts and session events record nothing and print nothing.
#[test]
fn kiro_non_edit_events_are_silent_no_ops() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("a.txt");
    file.assert_committed_lines(crate::lines!["a".unattributed_human()]);

    for payload in [
        envelope(&repo, "PreToolUse", "fs_read", json!({"path": "a.txt"})),
        envelope(&repo, "PostToolUse", "grep", json!({"pattern": "a"})),
        json!({"hook_event_name": "UserPromptSubmit", "cwd": repo.canonical_path().to_string_lossy(),
               "session_id": "s", "prompt": "hi"})
        .to_string(),
        json!({"hook_event_name": "Stop", "cwd": repo.canonical_path().to_string_lossy(),
               "session_id": "s"})
        .to_string(),
    ] {
        let out = hook(&repo, &payload);
        assert!(
            out.contains("checkpoint_requests=0"),
            "{payload}: expected no checkpoint, got {out}"
        );
    }
}
