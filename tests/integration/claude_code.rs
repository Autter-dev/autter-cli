use crate::test_utils::fixture_path;
use autter::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use autter::streams::agent::Agent;
use autter::streams::agents::ClaudeAgent;
use autter::streams::watermark::ByteOffsetWatermark;
use serde_json::json;
use std::fs;
use std::io::Write;

#[test]
fn test_claude_code_raw_event_fidelity() {
    let fixture = fixture_path("example-claude-code.jsonl");
    let agent = ClaudeAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Failed to parse JSONL");

    let expected: Vec<serde_json::Value> = std::fs::read_to_string(&fixture)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(result.events, expected);
}

#[test]
fn test_claude_preset_extracts_edited_filepath() {
    let hook_input = r##"{
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "permission_mode": "default",
        "session_id": "23aad27c-175d-427f-ac5f-a6830b8e6e65",
        "tool_input": {
            "file_path": "/Users/svarlamov/projects/testing-git/README.md",
            "new_string": "# Testing Git Repository",
            "old_string": "# Testing Git"
        },
        "tool_name": "Edit",
        "transcript_path": "tests/fixtures/example-claude-code.jsonl"
    }"##;

    let events = resolve_preset("claude")
        .unwrap()
        .parse(hook_input, "t_test")
        .expect("Failed to run ClaudePreset");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(!e.file_paths.is_empty());
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("README.md"))
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_claude_preset_no_filepath_when_tool_input_missing() {
    let hook_input = r##"{
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "session_id": "23aad27c-175d-427f-ac5f-a6830b8e6e65",
        "tool_name": "Read",
        "transcript_path": "tests/fixtures/example-claude-code.jsonl"
    }"##;

    let events = resolve_preset("claude")
        .unwrap()
        .parse(hook_input, "t_test")
        .expect("Failed to run ClaudePreset");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.file_paths.is_empty());
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_claude_preset_ignores_vscode_copilot_payload() {
    let hook_input = json!({
        "hookEventName": "PreToolUse",
        "cwd": "/Users/test/project",
        "toolName": "copilot_replaceString",
        "transcript_path": "/Users/test/Library/Application Support/Code/User/workspaceStorage/workspace-id/GitHub.copilot-chat/transcripts/copilot-session-1.jsonl",
        "toolInput": {
            "file_path": "/Users/test/project/src/main.ts"
        },
        "sessionId": "copilot-session-1",
        "model": "copilot/claude-sonnet-4"
    })
    .to_string();

    let result = resolve_preset("claude")
        .unwrap()
        .parse(&hook_input, "t_test");
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Skipping VS Code hook payload in Claude preset")
    );
}

#[test]
fn test_claude_preset_ignores_cursor_payload() {
    let hook_input = json!({
        "conversation_id": "dff2bf79-6a53-446c-be41-f33512532fb0",
        "model": "default",
        "tool_name": "Write",
        "tool_input": {
            "file_path": "/Users/test/project/jokes.csv"
        },
        "transcript_path": "/Users/test/.cursor/projects/Users-test-project/agent-transcripts/dff2bf79-6a53-446c-be41-f33512532fb0/dff2bf79-6a53-446c-be41-f33512532fb0.jsonl",
        "hook_event_name": "postToolUse",
        "cursor_version": "2.5.26",
        "workspace_roots": ["/Users/test/project"]
    })
    .to_string();

    let result = resolve_preset("claude")
        .unwrap()
        .parse(&hook_input, "t_test");
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Skipping Cursor hook payload in Claude preset")
    );
}

#[test]
fn test_claude_preset_does_not_ignore_when_transcript_path_is_claude() {
    let temp = tempfile::tempdir().unwrap();
    let claude_dir = temp.path().join(".claude").join("projects");
    fs::create_dir_all(&claude_dir).unwrap();

    let transcript_path = claude_dir.join("session.jsonl");
    let fixture = fixture_path("example-claude-code.jsonl");
    let mut dst = std::fs::File::create(&transcript_path).unwrap();
    let src = std::fs::read(fixture).unwrap();
    dst.write_all(&src).unwrap();

    let hook_input = json!({
        "hookEventName": "PostToolUse",
        "cwd": "/Users/test/project",
        "toolName": "copilot_replaceString",
        "toolInput": {
            "file_path": "/Users/test/project/src/main.ts"
        },
        "sessionId": "copilot-session-2",
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    let events = resolve_preset("claude")
        .unwrap()
        .parse(&hook_input, "t_test")
        .expect("Expected native Claude preset handling");

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "claude");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_claude_e2e_prefers_latest_checkpoint_for_prompts() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();

    // Enable prompt sharing for all repositories (empty blacklist = no exclusions)
    repo.patch_autter_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]); // No exclusions = share everywhere
    });

    let repo_root = repo.canonical_path();

    // Create initial file and commit
    let src_dir = repo_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("main.rs");
    fs::write(&file_path, "fn main() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Use a stable transcript path so both checkpoints share the same agent_id
    let transcript_path = repo_root.join("claude-session.jsonl");

    // First checkpoint: empty transcript (simulates race where data isn't ready yet)
    fs::write(&transcript_path, "").unwrap();
    let hook_input = json!({
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    // First AI edit and checkpoint with empty transcript/model
    fs::write(&file_path, "fn main() {}\n// ai line one\n").unwrap();
    repo.autter(&["checkpoint", "claude", "--hook-input", &hook_input])
        .unwrap();

    // Second AI edit with the real transcript content
    let fixture = fixture_path("example-claude-code.jsonl");
    fs::copy(&fixture, &transcript_path).unwrap();
    fs::write(&file_path, "fn main() {}\n// ai line one\n// ai line two\n").unwrap();
    repo.autter(&["checkpoint", "claude", "--hook-input", &hook_input])
        .unwrap();

    // Commit the changes
    let commit = repo.stage_all_and_commit("Add AI lines").unwrap();

    // We should have exactly one session record keyed by the claude agent_id
    assert_eq!(
        commit.authorship_log.metadata.sessions.len(),
        1,
        "Expected a single session record"
    );
    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Session record should exist");

    // Model is extracted from the real transcript fixture copied in the second checkpoint
    assert_eq!(
        session_record.agent_id.model, "claude-sonnet-4-20250514",
        "Session record model should come from the latest checkpoint's transcript"
    );
}

#[test]
fn test_claude_code_thinking_raw_event_fidelity() {
    let fixture = fixture_path("claude-code-with-thinking.jsonl");
    let agent = ClaudeAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Failed to parse JSONL");

    let expected: Vec<serde_json::Value> = std::fs::read_to_string(&fixture)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(result.events, expected);
}

#[test]
fn test_claude_code_plan_raw_event_fidelity() {
    let fixture = fixture_path("claude-code-with-plan.jsonl");
    let agent = ClaudeAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Failed to parse JSONL");

    let expected: Vec<serde_json::Value> = std::fs::read_to_string(&fixture)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(result.events, expected);
}

#[test]
#[serial_test::serial]
fn test_claude_subagent_checkpoint_sets_parent_session_id() {
    use crate::repos::test_repo::TestRepo;

    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("src").join("main.rs");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(&file_path, "fn main() {}\n").unwrap();

    let subagent_path = repo_root
        .join(".claude")
        .join("projects")
        .join("proj")
        .join("parent-uuid-abc")
        .join("subagents")
        .join("agent-xyz123.jsonl");
    std::fs::create_dir_all(subagent_path.parent().unwrap()).unwrap();
    std::fs::write(&subagent_path, "{\"type\":\"user\",\"message\":{\"content\":\"test\"},\"timestamp\":\"2026-01-01T00:00:00Z\"}\n").unwrap();

    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "agent-xyz123",
        "cwd": repo_root.to_string_lossy().to_string(),
        "transcript_path": subagent_path.to_string_lossy().to_string(),
        "tool_name": "Edit",
        "tool_use_id": "toolu_test_001",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    let preset = resolve_preset("claude").unwrap();
    let events = preset.parse(&hook_input, "t_test").unwrap();

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let ts = e
                .stream_source
                .as_ref()
                .expect("should have transcript source");
            assert_eq!(
                ts.external_parent_session_id,
                Some("parent-uuid-abc".to_string()),
                "Claude subagent checkpoint should detect parent session from path"
            );
            assert_eq!(ts.external_session_id, "agent-xyz123",);
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_claude_normal_session_checkpoint_has_no_parent() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let transcript_path = temp_dir.path().join("normal-session-uuid.jsonl");
    std::fs::write(&transcript_path, "{\"type\":\"user\",\"message\":{\"content\":\"test\"},\"timestamp\":\"2026-01-01T00:00:00Z\"}\n").unwrap();

    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "normal-session-uuid",
        "cwd": "/tmp/project",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_name": "Edit",
        "tool_use_id": "toolu_test_002",
        "tool_input": {
            "file_path": "/tmp/project/src/main.rs"
        }
    })
    .to_string();

    let preset = resolve_preset("claude").unwrap();
    let events = preset.parse(&hook_input, "t_test").unwrap();

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let ts = e
                .stream_source
                .as_ref()
                .expect("should have transcript source");
            assert_eq!(
                ts.external_parent_session_id, None,
                "Normal (non-subagent) Claude session should have no parent"
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

crate::reuse_tests_in_worktree!(
    test_claude_code_raw_event_fidelity,
    test_claude_code_thinking_raw_event_fidelity,
    test_claude_code_plan_raw_event_fidelity,
    test_claude_preset_extracts_edited_filepath,
    test_claude_preset_no_filepath_when_tool_input_missing,
    test_claude_preset_ignores_vscode_copilot_payload,
    test_claude_preset_ignores_cursor_payload,
    test_claude_preset_does_not_ignore_when_transcript_path_is_claude,
    test_claude_e2e_prefers_latest_checkpoint_for_prompts,
);

/// A prompt submitted or a turn ended changes the transcript without a file
/// edit: the hook wakes the daemon's transcript reader and records no
/// checkpoint, so read-only sessions reach the platform without waiting for
/// the periodic sweep.
#[test]
fn test_claude_prompt_and_stop_hooks_record_no_checkpoint() {
    use crate::repos::test_repo::TestRepo;

    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();
    fs::write(repo_root.join("README.md"), "# demo\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let transcript_path = repo_root.join("session.jsonl");
    fs::write(
        &transcript_path,
        "{\"type\":\"user\",\"message\":{\"content\":\"How do retries work?\"}}\n",
    )
    .unwrap();

    for event in ["UserPromptSubmit", "Stop", "SubagentStop"] {
        let hook = json!({
            "cwd": repo_root.to_string_lossy(),
            "hook_event_name": event,
            "session_id": "sess-activity-test",
            "transcript_path": transcript_path.to_string_lossy(),
            "prompt": "How do retries work?"
        })
        .to_string();
        let stdout = repo
            .autter(&["checkpoint", "claude", "--hook-input", &hook])
            .unwrap_or_else(|e| panic!("{event} hook should succeed: {e}"));
        assert!(
            stdout.contains("checkpoint_requests=0"),
            "{event}: no checkpoint, got: {stdout}"
        );
        assert!(
            stdout.contains("session_activity_requests=1"),
            "{event}: one wake-up, got: {stdout}"
        );
    }

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    assert!(checkpoints.is_empty(), "no checkpoint is written");
}

#[test]
fn test_claude_preset_parses_real_prompt_submit_payload() {
    let hook_input = r##"{
        "session_id": "23aad27c-175d-427f-ac5f-a6830b8e6e65",
        "transcript_path": "tests/fixtures/example-claude-code.jsonl",
        "cwd": "/Users/svarlamov/projects/testing-git",
        "permission_mode": "default",
        "hook_event_name": "UserPromptSubmit",
        "prompt": "Write a function to calculate the factorial of a number"
    }"##;

    let events = resolve_preset("claude")
        .unwrap()
        .parse(hook_input, "t_test")
        .expect("Failed to run ClaudePreset");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::SessionActivity(e) => {
            assert_eq!(
                e.stream_source.external_session_id,
                "23aad27c-175d-427f-ac5f-a6830b8e6e65"
            );
        }
        _ => panic!("Expected SessionActivity"),
    }
}

// ============================================================================
// The model on every checkpoint
// ============================================================================

const OPUS: &str = "claude-opus-4-7";

/// Session-start records Claude Code writes before the first assistant message
/// (hook progress, file-history snapshots). Enough of them that the first
/// assistant message is not among the first lines of the transcript.
fn claude_session_preamble(session_id: &str) -> String {
    let mut out = String::new();
    for i in 0..30 {
        out.push_str(
            &json!({
                "type": "progress",
                "data": {"type": "hook_progress", "hookEvent": "SessionStart", "index": i},
                "sessionId": session_id,
                "uuid": format!("progress-{i}"),
                "timestamp": "2026-11-08T10:00:00.000Z"
            })
            .to_string(),
        );
        out.push('\n');
    }
    out.push_str(
        &json!({
            "type": "user",
            "message": {"role": "user", "content": "Write the nine files"},
            "sessionId": session_id,
            "uuid": "prompt-1",
            "timestamp": "2026-11-08T10:00:01.000Z"
        })
        .to_string(),
    );
    out.push('\n');
    out
}

fn claude_tool_use_line(
    session_id: &str,
    tool_use_id: &str,
    file_path: &str,
    model: &str,
) -> String {
    let mut line = json!({
        "type": "assistant",
        "message": {
            "model": model,
            "id": "msg_batch",
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": tool_use_id,
                "name": "Write",
                "input": {"file_path": file_path, "content": "..."}
            }]
        },
        "sessionId": session_id,
        "uuid": format!("assistant-{tool_use_id}"),
        "timestamp": "2026-11-08T10:00:02.000Z"
    })
    .to_string();
    line.push('\n');
    line
}

/// A tool result carrying the whole written file, about 900 KB, the size of
/// the records in the report.
fn claude_large_tool_result_line(session_id: &str, tool_use_id: &str, file_path: &str) -> String {
    let big = "x".repeat(120).repeat(7_500);
    let mut line = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": "File created"}]
        },
        "toolUseResult": {"type": "create", "filePath": file_path, "content": big},
        "sessionId": session_id,
        "uuid": format!("result-{tool_use_id}"),
        "timestamp": "2026-11-08T10:00:03.000Z"
    })
    .to_string();
    line.push('\n');
    line
}

fn append(path: &std::path::Path, text: &str) {
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

fn claude_hook(
    event: &str,
    cwd: &std::path::Path,
    session_id: &str,
    transcript: &std::path::Path,
    tool_use_id: &str,
    file_path: &std::path::Path,
) -> String {
    json!({
        "session_id": session_id,
        "transcript_path": transcript.to_string_lossy(),
        "cwd": cwd.to_string_lossy(),
        "hook_event_name": event,
        "tool_name": "Write",
        "tool_use_id": tool_use_id,
        "tool_input": {"file_path": file_path.to_string_lossy(), "content": "..."}
    })
    .to_string()
}

fn ai_checkpoint_models(repo: &crate::repos::test_repo::TestRepo) -> Vec<String> {
    repo.current_working_logs()
        .read_all_checkpoints()
        .unwrap()
        .into_iter()
        .filter(|c| c.kind.is_ai())
        .map(|c| c.agent_id.expect("AI checkpoint has an agent").model)
        .collect()
}

/// Nine consecutive Writes from one assistant turn, with a ~900 KB tool result
/// written to the transcript after each and no assistant message in between.
/// The model was read from a fixed 50 KB tail of the transcript, so from the
/// second write on it found only tool-result bytes and recorded
/// `claude/unknown-model`. Every checkpoint and the note must name the model.
#[test]
fn test_claude_rapid_consecutive_writes_keep_model() {
    use crate::repos::test_file::ExpectedLineExt;
    use crate::repos::test_repo::TestRepo;

    let repo = TestRepo::new();
    let root = repo.canonical_path();
    fs::write(root.join("README.md"), "readme\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.filename("README.md")
        .assert_committed_lines(crate::lines!["readme".unattributed_human()]);

    let transcript_dir = tempfile::tempdir().unwrap();
    let transcript = transcript_dir.path().join("rapid-session.jsonl");
    let session_id = "rapid-session";
    let files: Vec<_> = (1..=9).map(|k| root.join(format!("f{k}.txt"))).collect();

    // The assistant issues all nine Writes in one message.
    let mut text = claude_session_preamble(session_id);
    for (k, file) in files.iter().enumerate() {
        text.push_str(&claude_tool_use_line(
            session_id,
            &format!("toolu_{k}"),
            &file.to_string_lossy(),
            OPUS,
        ));
    }
    fs::write(&transcript, text).unwrap();

    for (k, file) in files.iter().enumerate() {
        let tool_use_id = format!("toolu_{k}");
        let pre = claude_hook(
            "PreToolUse",
            &root,
            session_id,
            &transcript,
            &tool_use_id,
            file,
        );
        repo.autter(&["checkpoint", "claude", "--hook-input", &pre])
            .unwrap();
        fs::write(file, format!("ai line {}\n", k + 1)).unwrap();
        let post = claude_hook(
            "PostToolUse",
            &root,
            session_id,
            &transcript,
            &tool_use_id,
            file,
        );
        repo.autter(&["checkpoint", "claude", "--hook-input", &post])
            .unwrap();
        append(
            &transcript,
            &claude_large_tool_result_line(session_id, &tool_use_id, &file.to_string_lossy()),
        );
    }

    let models = ai_checkpoint_models(&repo);
    assert_eq!(models.len(), 9, "one AI checkpoint per write: {models:?}");
    assert!(
        models.iter().all(|m| m == OPUS),
        "every checkpoint names the model: {models:?}"
    );

    let commit = repo.stage_all_and_commit("Nine AI files").unwrap();
    let sessions = &commit.authorship_log.metadata.sessions;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions.values().next().unwrap().agent_id.model, OPUS);
    for k in 1..=9 {
        repo.filename(&format!("f{k}.txt"))
            .assert_committed_lines(crate::lines![format!("ai line {k}").ai()]);
    }
}

/// Checkpoints taken before the transcript names the model are filled in once
/// it does, and a later checkpoint whose transcript shows no model (a new
/// working log after the commit) takes the session's last known model.
#[test]
fn test_claude_unknown_model_is_backfilled_and_remembered() {
    use crate::repos::test_file::ExpectedLineExt;
    use crate::repos::test_repo::TestRepo;

    let repo = TestRepo::new();
    let root = repo.canonical_path();
    fs::write(root.join("README.md"), "readme\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.filename("README.md")
        .assert_committed_lines(crate::lines!["readme".unattributed_human()]);

    let transcript_dir = tempfile::tempdir().unwrap();
    let transcript = transcript_dir.path().join("late-session.jsonl");
    let session_id = "late-session";
    let preamble = claude_session_preamble(session_id);
    fs::write(&transcript, &preamble).unwrap();

    let edit = |name: &str, tool_use_id: &str, content: &str| {
        let file = root.join(name);
        let pre = claude_hook(
            "PreToolUse",
            &root,
            session_id,
            &transcript,
            tool_use_id,
            &file,
        );
        repo.autter(&["checkpoint", "claude", "--hook-input", &pre])
            .unwrap();
        fs::write(&file, content).unwrap();
        let post = claude_hook(
            "PostToolUse",
            &root,
            session_id,
            &transcript,
            tool_use_id,
            &file,
        );
        repo.autter(&["checkpoint", "claude", "--hook-input", &post])
            .unwrap();
    };

    // The transcript has no assistant message yet: the model is unknown.
    edit("a.txt", "toolu_a", "ai a\n");
    assert_eq!(ai_checkpoint_models(&repo), vec!["claude/unknown-model"]);

    // Now it names the model: the earlier checkpoint is backfilled.
    append(
        &transcript,
        &claude_tool_use_line(
            session_id,
            "toolu_b",
            &root.join("b.txt").to_string_lossy(),
            OPUS,
        ),
    );
    edit("b.txt", "toolu_b", "ai b\n");
    assert_eq!(ai_checkpoint_models(&repo), vec![OPUS, OPUS]);

    let commit = repo.stage_all_and_commit("Two AI files").unwrap();
    let sessions = &commit.authorship_log.metadata.sessions;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions.values().next().unwrap().agent_id.model, OPUS);
    repo.filename("a.txt")
        .assert_committed_lines(crate::lines!["ai a".ai()]);
    repo.filename("b.txt")
        .assert_committed_lines(crate::lines!["ai b".ai()]);

    // The transcript again shows no model (rewritten, or not flushed yet):
    // the session's last known model is used.
    fs::write(&transcript, &preamble).unwrap();
    edit("c.txt", "toolu_c", "ai c\n");
    assert_eq!(ai_checkpoint_models(&repo), vec![OPUS]);

    let commit = repo.stage_all_and_commit("Third AI file").unwrap();
    let sessions = &commit.authorship_log.metadata.sessions;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions.values().next().unwrap().agent_id.model, OPUS);
    repo.filename("c.txt")
        .assert_committed_lines(crate::lines!["ai c".ai()]);
}
