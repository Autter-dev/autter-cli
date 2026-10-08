//! Google Antigravity hooks: edits reported through the documented
//! PreToolUse/PostToolUse payload (https://antigravity.google/docs/hooks) are
//! attributed to the agent. Installing into ~/.gemini/config/hooks.json is
//! covered in-process by the installer's unit tests (running `install-hooks`
//! here would drive real editor CLIs found on PATH).

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use serde_json::json;
use std::fs;

fn hook_payload(repo: &TestRepo, tool: &str, target: &std::path::Path) -> String {
    json!({
        "toolCall": {"name": tool, "args": {"TargetFile": target.to_string_lossy()}},
        "stepIdx": 4,
        "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
        "workspacePaths": [repo.canonical_path().to_string_lossy()],
        "transcriptPath": "/tmp/does-not-exist/transcript.jsonl",
        "modelName": "gemini-3.6-flash-medium"
    })
    .to_string()
}

#[test]
fn antigravity_hook_edits_are_attributed_to_ai() {
    let repo = TestRepo::new();
    let file_path = repo.canonical_path().join("agent.rs");
    fs::write(&file_path, "fn human() {}\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // PreToolUse before the agent writes...
    let pre = repo
        .autter(&[
            "checkpoint",
            "antigravity-pre",
            "--hook-input",
            &hook_payload(&repo, "replace_file_content", &file_path),
        ])
        .unwrap();
    assert!(
        pre.trim_start().starts_with("{}"),
        "hook must reply {{}}: {pre}"
    );

    fs::write(&file_path, "fn human() {}\nfn agent() {}\n").unwrap();

    // ...and PostToolUse after.
    repo.autter(&[
        "checkpoint",
        "antigravity-post",
        "--hook-input",
        &hook_payload(&repo, "replace_file_content", &file_path),
    ])
    .unwrap();

    repo.stage_all_and_commit("agent edit").unwrap();
    let mut file = repo.filename("agent.rs");
    file.assert_committed_lines(lines![
        "fn human() {}".unattributed_human(),
        "fn agent() {}".ai(),
    ]);
}
