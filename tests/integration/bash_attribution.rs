use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::fixture_path;
use serde_json::json;
use std::fs;

#[test]
fn test_bash_pre_human_checkpoint_preserves_dirty_file_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("example.txt");

    let initial = "original line\n";
    fs::write(&file_path, initial).unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file = repo.filename("example.txt");
    file.assert_committed_lines(lines!["original line".unattributed_human()]);

    let after_human_edit = "original line\nhuman edit line\n";
    fs::write(&file_path, after_human_edit).unwrap();
    repo.autter(&["checkpoint", "human", "example.txt"])
        .unwrap();

    let after_bash = "original line\nhuman edit line\nai bash line\n";
    fs::write(&file_path, after_bash).unwrap();
    repo.autter(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();

    repo.stage_all_and_commit("After bash").unwrap();
    file.assert_committed_lines(lines![
        "original line".unattributed_human(),
        "human edit line".unattributed_human(),
        "ai bash line".ai(),
    ]);
}

#[test]
fn test_bash_clean_files_only_bash_changes_get_ai_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("clean.txt");

    let initial = "committed line\n";
    fs::write(&file_path, initial).unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file = repo.filename("clean.txt");
    file.assert_committed_lines(lines!["committed line".unattributed_human()]);

    repo.autter(&["checkpoint", "human", "clean.txt"]).unwrap();

    let after_bash = "committed line\nbash added this\n";
    fs::write(&file_path, after_bash).unwrap();
    repo.autter(&["checkpoint", "mock_ai", "clean.txt"])
        .unwrap();

    repo.stage_all_and_commit("After bash").unwrap();
    file.assert_committed_lines(lines![
        "committed line".unattributed_human(),
        "bash added this".ai(),
    ]);
}

#[test]
fn test_bash_multiple_files_mixed_dirty_state() {
    let repo = TestRepo::new();
    let a_path = repo.path().join("a.txt");
    let b_path = repo.path().join("b.txt");

    fs::write(&a_path, "line a\n").unwrap();
    fs::write(&b_path, "line b\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file_a = repo.filename("a.txt");
    let mut file_b = repo.filename("b.txt");
    file_a.assert_committed_lines(lines!["line a".unattributed_human()]);
    file_b.assert_committed_lines(lines!["line b".unattributed_human()]);

    fs::write(&a_path, "line a\nhuman touched a\n").unwrap();

    repo.autter(&["checkpoint", "human", "a.txt"]).unwrap();
    repo.autter(&["checkpoint", "human", "b.txt"]).unwrap();

    fs::write(&a_path, "line a\nhuman touched a\nbash touched a\n").unwrap();
    fs::write(&b_path, "line b\nbash touched b\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "a.txt"]).unwrap();
    repo.autter(&["checkpoint", "mock_ai", "b.txt"]).unwrap();

    repo.stage_all_and_commit("After bash").unwrap();
    file_a.assert_committed_lines(lines![
        "line a".unattributed_human(),
        "human touched a".unattributed_human(),
        "bash touched a".ai(),
    ]);
    file_b.assert_committed_lines(lines!["line b".unattributed_human(), "bash touched b".ai()]);
}

/// Orchestrator-level regression test: fires through the real codex
/// preset/orchestrator path (not manual `autter checkpoint human` CLI
/// calls). If `execute_pre_bash_call` regresses to returning `Ok(vec![])`,
/// the dirty human edit would be swept into the post-bash AI checkpoint
/// and misattributed as AI.
#[test]
fn test_codex_preset_pre_bash_preserves_dirty_file_attribution() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("example.txt");

    fs::write(&file_path, "original line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file = repo.filename("example.txt");
    file.assert_committed_lines(lines!["original line".unattributed_human()]);

    // Human edits the file before the AI bash tool runs.
    fs::write(&file_path, "original line\nhuman edit line\n").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-transcript.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "attr-pre-sess",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "attr-bash-1",
        "tool_input": { "command": "echo hello" },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.autter(&["checkpoint", "codex", "--hook-input", &pre_hook_input])
        .expect("codex pre-hook checkpoint should succeed");

    // AI bash tool edits the file.
    fs::write(&file_path, "original line\nhuman edit line\nai bash line\n").unwrap();

    let post_hook_input = json!({
        "session_id": "attr-pre-sess",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "attr-bash-1",
        "tool_input": { "command": "echo hello" },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.autter(&["checkpoint", "codex", "--hook-input", &post_hook_input])
        .expect("codex post-hook checkpoint should succeed");

    repo.stage_all_and_commit("After codex bash").unwrap();
    file.assert_committed_lines(lines![
        "original line".unattributed_human(),
        "human edit line".unattributed_human(),
        "ai bash line".ai(),
    ]);
}

// ---------------------------------------------------------------------------
// Claude Code Bash tool: edits made through shell commands
// ---------------------------------------------------------------------------
//
// Regression tests for "a line Claude Code wrote with a shell command came out
// 100% untracked" (a 3-line PR with 1 human and 2 Claude lines reported 33.3%
// AI instead of 66.7%). They fire the real Claude preset's PreToolUse and
// PostToolUse hooks for the Bash tool around a real shell command.

fn claude_bash_hook(
    repo_root: &std::path::Path,
    transcript_path: &std::path::Path,
    event: &str,
    tool_use_id: &str,
    command: &str,
) -> String {
    json!({
        "session_id": "claude-shell-sess",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": event,
        "tool_name": "Bash",
        "tool_use_id": tool_use_id,
        "tool_input": { "command": command },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string()
}

/// Run `command` as Claude Code's Bash tool would: PreToolUse hook, the
/// shell command itself, then the PostToolUse hook.
fn run_claude_bash(repo: &TestRepo, tool_use_id: &str, command: &str) {
    let repo_root = repo.canonical_path();
    let transcript_path = repo_root.join("claude-transcript.jsonl");
    if !transcript_path.exists() {
        fs::copy(fixture_path("example-claude-code.jsonl"), &transcript_path).unwrap();
    }
    let pre = claude_bash_hook(
        &repo_root,
        &transcript_path,
        "PreToolUse",
        tool_use_id,
        command,
    );
    repo.autter(&["checkpoint", "claude", "--hook-input", &pre])
        .expect("claude Bash pre-hook should succeed");

    let output = std::process::Command::new("sh")
        .args(["-c", command])
        .current_dir(&repo_root)
        .output()
        .expect("shell command should run");
    assert!(
        output.status.success(),
        "shell command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let post = claude_bash_hook(
        &repo_root,
        &transcript_path,
        "PostToolUse",
        tool_use_id,
        command,
    );
    repo.autter(&["checkpoint", "claude", "--hook-input", &post])
        .expect("claude Bash post-hook should succeed");
}

/// Commit only `rel_path` so the copied transcript never lands in a commit.
fn commit_file(repo: &TestRepo, rel_path: &str, message: &str) {
    repo.git(&["add", rel_path]).unwrap();
    repo.commit(message).unwrap();
}

#[test]
fn test_claude_bash_echo_append_is_attributed_to_ai() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("notes.md");

    // Line 1: written by the human, captured by the editor extension.
    fs::write(&file_path, "human line\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "notes.md"])
        .unwrap();
    commit_file(&repo, "notes.md", "Human line");
    let mut file = repo.filename("notes.md");
    file.assert_committed_lines(lines!["human line".human()]);

    // Line 2: Claude's Edit tool (pre = untracked checkpoint, post = AI).
    repo.autter(&["checkpoint", "human", "notes.md"]).unwrap();
    fs::write(&file_path, "human line\nclaude edit line\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "notes.md"]).unwrap();

    // Line 3: Claude's Bash tool, immediately after the Edit.
    run_claude_bash(&repo, "bash-echo-1", "echo 'claude shell line' >> notes.md");

    commit_file(&repo, "notes.md", "Claude lines");
    file.assert_committed_lines(lines![
        "human line".human(),
        "claude edit line".ai(),
        "claude shell line".ai(),
    ]);
}

#[test]
fn test_claude_bash_sed_in_place_is_attributed_to_ai() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("config.txt");

    fs::write(&file_path, "name = old\nkeep = this\n").unwrap();
    commit_file(&repo, "config.txt", "Initial commit");
    let mut file = repo.filename("config.txt");
    file.assert_committed_lines(lines![
        "name = old".unattributed_human(),
        "keep = this".unattributed_human(),
    ]);

    // `-i.bak` works with both GNU and BSD sed.
    run_claude_bash(
        &repo,
        "bash-sed-1",
        "sed -i.bak 's/name = old/name = new/' config.txt && rm config.txt.bak",
    );

    commit_file(&repo, "config.txt", "Claude sed edit");
    file.assert_committed_lines(lines![
        "name = new".ai(),
        "keep = this".unattributed_human(),
    ]);
}

#[test]
fn test_claude_bash_formatter_rewrite_is_attributed_to_ai() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("app.js");

    fs::write(&file_path, "const a = 1;\nconst b = 2;\n").unwrap();
    commit_file(&repo, "app.js", "Initial commit");
    let mut file = repo.filename("app.js");
    file.assert_committed_lines(lines![
        "const a = 1;".unattributed_human(),
        "const b = 2;".unattributed_human(),
    ]);

    // A formatter writes a new file and renames it over the original (new
    // inode), changing only the lines it reformats.
    run_claude_bash(
        &repo,
        "bash-fmt-1",
        "awk '{ sub(/const b = 2;/, \"const b = 2; // formatted\"); print }' app.js > app.js.tmp && mv app.js.tmp app.js",
    );

    commit_file(&repo, "app.js", "Claude formatter run");
    file.assert_committed_lines(lines![
        "const a = 1;".unattributed_human(),
        "const b = 2; // formatted".ai(),
    ]);
}

#[test]
fn test_claude_bash_does_not_claim_human_edit_made_just_before() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("shared.txt");

    fs::write(&file_path, "base line\n").unwrap();
    commit_file(&repo, "shared.txt", "Initial commit");
    let mut file = repo.filename("shared.txt");
    file.assert_committed_lines(lines!["base line".unattributed_human()]);

    // Someone else edits the file with no checkpoint, right before Claude's
    // Bash call. The pre-hook must record it as untracked, not leave it for
    // the post-hook to sweep into the AI checkpoint.
    fs::write(&file_path, "base line\nunchecked human line\n").unwrap();

    run_claude_bash(
        &repo,
        "bash-echo-2",
        "echo 'claude shell line' >> shared.txt",
    );

    commit_file(&repo, "shared.txt", "After Claude bash");
    file.assert_committed_lines(lines![
        "base line".unattributed_human(),
        "unchecked human line".unattributed_human(),
        "claude shell line".ai(),
    ]);
}
