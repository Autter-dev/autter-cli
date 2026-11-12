//! Kiro hook preset (Kiro CLI v3).
//!
//! Kiro runs command hooks around agent tool calls. autter installs one file,
//! `~/.kiro/hooks/autter.json`, whose `PreToolUse` and `PostToolUse` hooks
//! (matchers `write` and `shell`) run `autter checkpoint kiro --hook-input
//! stdin`. Sources (fetched 2026-11-12):
//! - https://kiro.dev/docs/hooks/ : standalone hook files
//!   (`{"version": "v1", "hooks": [...]}`), and "Kiro CLI V3 also loads
//!   global Hooks from `~/.kiro/hooks/`".
//! - https://kiro.dev/docs/hooks/types/ : triggers and the stdin envelope
//!   (`hook_event_name`, `cwd`, `session_id`, `tool_name`, `tool_input`).
//! - https://kiro.dev/docs/cli/v3/hooks-migration/ : the `write` matcher
//!   covers `fs_write` and `code`, `shell` covers `execute_bash`.
//! - https://kiro.dev/docs/hooks/actions/ : on exit 0 a command's stdout "is
//!   added to the agent's context", and a failing `PreToolUse` command
//!   blocks the tool. autter therefore never prints to stdout from this
//!   preset and always exits 0 in hook mode.
//!
//! UNCONFIRMED until a real capture (see docs/kiro-capture.md): which
//! `tool_input` field names the edited file, whether `PostToolUse` repeats
//! `tool_input` (a Kiro IDE capture published by AWS Labs shows it empty,
//! with the path only in `tool_response` prose:
//! https://awslabs.github.io/aidlc-workflows/reference/kiro-ide-hook-payload/),
//! whether a tool-use id or a model name is sent, and the tool names Kiro CLI
//! v3 uses. Every guess sits in one function below
//! ([`classify_kiro_tool`], [`edited_paths`], [`paths_from_tool_response`],
//! [`tool_use_id`], [`model_name`]) so a capture changes one place.

use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::authorship::working_log::AgentId;
use crate::error::AutterError;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct KiroPreset;

/// Agent tool name recorded in attribution.
const TOOL: &str = "kiro";

/// Session id used when a payload has none (the documented envelope always
/// carries one; this keeps an odd payload from losing the edit).
const UNKNOWN_SESSION: &str = "kiro-unknown-session";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pre,
    Post,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KiroToolClass {
    /// Writes named files: checkpointed by path.
    FileEdit,
    /// Shell commands, and tools that can touch any number of files: the
    /// files are found by comparing the repository before and after.
    Snapshot,
    /// Reads, searches, web: nothing to record.
    Skip,
}

/// Tool names, UNCONFIRMED for Kiro CLI v3. From the docs: `fs_write` (alias
/// `fsWrite`, `write`) and `code` under the `write` matcher, `execute_bash`
/// (alias `execute_cmd`, `shell`) under `shell`. From the AWS Labs IDE
/// capture: `str_replace`, `fs_append`, `create_file`, `edit_file`,
/// `apply_patch`, `delete_file`, `execute_pwsh`. `code` rewrites by pattern
/// across files, so it is compared before and after rather than trusted to
/// name its files.
fn classify_kiro_tool(tool_name: &str) -> KiroToolClass {
    match tool_name {
        "fs_write" | "fsWrite" | "write" | "str_replace" | "strReplace" | "fs_append"
        | "fsAppend" | "create_file" | "edit_file" | "apply_patch" | "delete_file" => {
            KiroToolClass::FileEdit
        }
        "execute_bash" | "executeBash" | "execute_cmd" | "execute_pwsh" | "shell" | "code" => {
            KiroToolClass::Snapshot
        }
        _ => KiroToolClass::Skip,
    }
}

/// Files a write tool names in `tool_input`, UNCONFIRMED field names: Q
/// Developer CLI's `fs_write` (which Kiro CLI grew from) uses `path`; the
/// IDE capture shows `path` (PreToolUse), `targetFile` (`delete_file`); other
/// agents use `file_path` / `filePath` / `target_file`; `paths` and
/// `file_paths` cover multi-file tools.
fn edited_paths(tool_input: &Value) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        let trimmed = raw.trim();
        if !trimmed.is_empty() && !paths.iter().any(|p| p == trimmed) {
            paths.push(trimmed.to_string());
        }
    };
    for key in [
        "path",
        "file_path",
        "filePath",
        "target_file",
        "targetFile",
        "TargetFile",
    ] {
        if let Some(s) = tool_input.get(key).and_then(Value::as_str) {
            push(s);
        }
    }
    for key in ["paths", "file_paths", "filePaths"] {
        if let Some(list) = tool_input.get(key).and_then(Value::as_array) {
            for s in list.iter().filter_map(Value::as_str) {
                push(s);
            }
        }
    }
    paths
}

/// Files named in a `tool_response` sentence, for `PostToolUse` payloads whose
/// `tool_input` is empty. Phrasings from the AWS Labs Kiro IDE capture,
/// UNCONFIRMED for Kiro CLI v3: "Created the <PATH> file.", "Replaced text in
/// <PATH>", "Appended the text to the <PATH> file.".
fn paths_from_tool_response(response: &Value) -> Vec<String> {
    let text = match response {
        Value::String(s) => s.clone(),
        Value::Object(_) | Value::Array(_) => response.to_string(),
        _ => return Vec::new(),
    };
    let mut paths: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let found = [
            ("Created the ", " file"),
            ("Appended the text to the ", " file"),
            ("Overwrote the ", " file"),
            ("Deleted the ", " file"),
        ]
        .iter()
        .find_map(|(start, end)| {
            let rest = line.split_once(start)?.1;
            Some(rest.rsplit_once(end).map_or(rest, |(p, _)| p).to_string())
        })
        .or_else(|| {
            let rest = line.split_once("Replaced text in ")?.1;
            Some(rest.trim_end_matches(['.', ' ']).to_string())
        });
        if let Some(path) = found {
            let path = path
                .trim()
                .trim_matches(['`', '\'', '"'])
                .trim()
                .to_string();
            if !path.is_empty() && !path.contains(' ') && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// Pairs a pre and a post call. UNCONFIRMED: Kiro does not document a
/// tool-use id; when one is sent under a common name it is used, otherwise
/// the session id and tool name (calls in one session run one at a time).
fn tool_use_id(data: &Value, session_id: &str, tool_name: &str) -> String {
    ["tool_use_id", "toolUseId", "tool_call_id", "toolCallId"]
        .iter()
        .find_map(|key| {
            data.get(*key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(|id| format!("{session_id}:{id}"))
        .unwrap_or_else(|| format!("{session_id}:{tool_name}"))
}

/// UNCONFIRMED: Kiro documents no model field. Recorded as `unknown` unless a
/// payload carries one under a common name.
fn model_name(data: &Value) -> String {
    ["model", "model_id", "modelId", "model_name", "modelName"]
        .iter()
        .find_map(|key| {
            data.get(*key)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or("unknown")
        .to_string()
}

fn absolute(path: &str, cwd: &Path) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() { p } else { cwd.join(p) }
}

impl AgentPreset for KiroPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, AutterError> {
        let data: Value = serde_json::from_str(hook_input)
            .map_err(|e| AutterError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;
        if !data.is_object() {
            return Err(AutterError::PresetError(
                "hook_input is not a JSON object".to_string(),
            ));
        }

        let event = data
            .get("hook_event_name")
            .or_else(|| data.get("hookEventName"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let phase = match event {
            "PreToolUse" | "preToolUse" => Phase::Pre,
            "PostToolUse" | "postToolUse" => Phase::Post,
            // Prompt submit, stop, session start, file events: nothing to
            // record (Kiro's session store is not read yet).
            _ => return Ok(Vec::new()),
        };

        let tool_name = data
            .get("tool_name")
            .or_else(|| data.get("toolName"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let class = classify_kiro_tool(tool_name);
        if class == KiroToolClass::Skip {
            return Ok(Vec::new());
        }

        let cwd = data
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| AutterError::PresetError("cwd not found in hook input".to_string()))?;
        let session_id = data
            .get("session_id")
            .or_else(|| data.get("sessionId"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(UNKNOWN_SESSION)
            .to_string();
        let tool_use = tool_use_id(&data, &session_id, tool_name);

        let mut metadata = HashMap::new();
        metadata.insert("kiro_tool_name".to_string(), tool_name.to_string());
        let context = PresetContext {
            agent_id: AgentId {
                tool: TOOL.to_string(),
                id: session_id.clone(),
                model: model_name(&data),
            },
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: cwd.clone(),
            metadata,
        };

        let event = match class {
            KiroToolClass::Snapshot => match phase {
                Phase::Pre => ParsedHookEvent::PreBashCall(PreBashCall {
                    context,
                    tool_use_id: tool_use,
                }),
                Phase::Post => ParsedHookEvent::PostBashCall(PostBashCall {
                    context,
                    tool_use_id: tool_use,
                    stream_source: None,
                }),
            },
            KiroToolClass::FileEdit => {
                let tool_input = data
                    .get("tool_input")
                    .or_else(|| data.get("toolInput"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let mut names = edited_paths(&tool_input);
                if names.is_empty() && phase == Phase::Post {
                    let response = data
                        .get("tool_response")
                        .or_else(|| data.get("toolResponse"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    names = paths_from_tool_response(&response);
                }
                // No path at all: nothing is recorded for this call (the
                // lines stay untracked), rather than guessing which files
                // changed. A capture showing such payloads would move these
                // tools to `Snapshot`.
                let file_paths: Vec<PathBuf> = names.iter().map(|n| absolute(n, &cwd)).collect();
                match phase {
                    Phase::Pre => ParsedHookEvent::PreFileEdit(PreFileEdit {
                        context,
                        file_paths,
                        dirty_files: None,
                        tool_use_id: Some(tool_use),
                    }),
                    Phase::Post => ParsedHookEvent::PostFileEdit(PostFileEdit {
                        context,
                        file_paths,
                        dirty_files: None,
                        stream_source: None,
                        tool_use_id: Some(tool_use),
                    }),
                }
            }
            KiroToolClass::Skip => unreachable!("skipped above"),
        };
        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TRACE: &str = "t_test123456789a";

    fn call(event: &str, tool: &str, input: Value) -> String {
        // Envelope from https://kiro.dev/docs/hooks/types/.
        json!({
            "hook_event_name": event,
            "cwd": "/workspace/project",
            "session_id": "abc123-def456-789",
            "tool_name": tool,
            "tool_input": input,
        })
        .to_string()
    }

    fn file_paths(events: &[ParsedHookEvent]) -> Vec<PathBuf> {
        match events {
            [ParsedHookEvent::PreFileEdit(e)] => e.file_paths.clone(),
            [ParsedHookEvent::PostFileEdit(e)] => e.file_paths.clone(),
            other => panic!("expected one file edit event, got {other:?}"),
        }
    }

    #[test]
    fn every_candidate_path_field_is_read() {
        for (key, value) in [
            ("path", json!("src/a.rs")),
            ("file_path", json!("src/a.rs")),
            ("filePath", json!("src/a.rs")),
            ("target_file", json!("src/a.rs")),
            ("targetFile", json!("src/a.rs")),
            ("TargetFile", json!("src/a.rs")),
            ("paths", json!(["src/a.rs"])),
            ("file_paths", json!(["src/a.rs"])),
            ("filePaths", json!(["src/a.rs"])),
        ] {
            let input = call("PreToolUse", "fs_write", json!({ key: value }));
            let events = KiroPreset.parse(&input, TRACE).unwrap();
            assert_eq!(
                file_paths(&events),
                vec![PathBuf::from("/workspace/project/src/a.rs")],
                "{key}"
            );
        }
    }

    #[test]
    fn absolute_paths_are_kept_and_duplicates_dropped() {
        let input = call(
            "PostToolUse",
            "fs_write",
            json!({"path": "/abs/b.rs", "file_path": "/abs/b.rs", "paths": ["/abs/b.rs", "c.rs"]}),
        );
        let events = KiroPreset.parse(&input, TRACE).unwrap();
        assert_eq!(
            file_paths(&events),
            vec![
                PathBuf::from("/abs/b.rs"),
                PathBuf::from("/workspace/project/c.rs")
            ]
        );
    }

    #[test]
    fn pre_and_post_map_to_file_edits_with_the_kiro_agent() {
        let pre = call(
            "PreToolUse",
            "fs_write",
            json!({"command": "create", "path": "a.txt", "file_text": "x"}),
        );
        match &KiroPreset.parse(&pre, TRACE).unwrap()[..] {
            [ParsedHookEvent::PreFileEdit(e)] => {
                assert_eq!(e.context.agent_id.tool, "kiro");
                assert_eq!(e.context.agent_id.id, "abc123-def456-789");
                assert_eq!(e.context.agent_id.model, "unknown");
                assert_eq!(e.context.cwd, PathBuf::from("/workspace/project"));
                assert_eq!(e.tool_use_id.as_deref(), Some("abc123-def456-789:fs_write"));
            }
            other => panic!("expected PreFileEdit, got {other:?}"),
        }
        let post = call("PostToolUse", "fs_write", json!({"path": "a.txt"}));
        assert!(matches!(
            &KiroPreset.parse(&post, TRACE).unwrap()[..],
            [ParsedHookEvent::PostFileEdit(_)]
        ));
    }

    #[test]
    fn post_without_tool_input_reads_the_path_from_the_response() {
        for (response, expected) in [
            (
                "Created the src/new.rs file.",
                "/workspace/project/src/new.rs",
            ),
            ("Replaced text in /abs/lib.rs", "/abs/lib.rs"),
            (
                "Appended the text to the notes.md file.",
                "/workspace/project/notes.md",
            ),
        ] {
            let input = json!({
                "hook_event_name": "PostToolUse",
                "cwd": "/workspace/project",
                "session_id": "s",
                "tool_name": "fs_write",
                "tool_input": {},
                "tool_response": response,
            })
            .to_string();
            let events = KiroPreset.parse(&input, TRACE).unwrap();
            assert_eq!(
                file_paths(&events),
                vec![PathBuf::from(expected)],
                "{response}"
            );
        }
    }

    #[test]
    fn a_write_with_no_path_records_nothing() {
        let input = call("PostToolUse", "fs_write", json!({}));
        assert!(file_paths(&KiroPreset.parse(&input, TRACE).unwrap()).is_empty());
        let input = call("PreToolUse", "fs_write", Value::Null);
        assert!(file_paths(&KiroPreset.parse(&input, TRACE).unwrap()).is_empty());
    }

    #[test]
    fn shell_and_code_compare_the_repository_before_and_after() {
        for tool in ["execute_bash", "execute_cmd", "execute_pwsh", "code"] {
            let pre = call("PreToolUse", tool, json!({"command": "cargo fmt"}));
            match &KiroPreset.parse(&pre, TRACE).unwrap()[..] {
                [ParsedHookEvent::PreBashCall(e)] => {
                    assert_eq!(e.tool_use_id, format!("abc123-def456-789:{tool}"));
                }
                other => panic!("{tool}: expected PreBashCall, got {other:?}"),
            }
            let post = call("PostToolUse", tool, json!({"command": "cargo fmt"}));
            assert!(matches!(
                &KiroPreset.parse(&post, TRACE).unwrap()[..],
                [ParsedHookEvent::PostBashCall(_)]
            ));
        }
    }

    #[test]
    fn a_sent_tool_use_id_and_model_are_used() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/w",
            "session_id": "s1",
            "tool_name": "fs_write",
            "tool_use_id": "tu_9",
            "model": "claude-sonnet-4.5",
            "tool_input": {"path": "a.rs"},
        })
        .to_string();
        match &KiroPreset.parse(&input, TRACE).unwrap()[..] {
            [ParsedHookEvent::PreFileEdit(e)] => {
                assert_eq!(e.tool_use_id.as_deref(), Some("s1:tu_9"));
                assert_eq!(e.context.agent_id.model, "claude-sonnet-4.5");
            }
            other => panic!("expected PreFileEdit, got {other:?}"),
        }
    }

    #[test]
    fn reads_other_events_and_unknown_tools_are_ignored() {
        for (event, tool) in [
            ("PreToolUse", "fs_read"),
            ("PostToolUse", "grep"),
            ("PreToolUse", "web_fetch"),
            ("UserPromptSubmit", "fs_write"),
            ("Stop", ""),
            ("SessionStart", ""),
        ] {
            let input = call(event, tool, json!({"path": "a.rs"}));
            assert!(
                KiroPreset.parse(&input, TRACE).unwrap().is_empty(),
                "{event} {tool}"
            );
        }
    }

    #[test]
    fn garbage_input_is_an_error_not_a_panic() {
        for input in ["", "not json", "[]", "42", "{\"hook_event_name\": 3}"] {
            let _ = KiroPreset.parse(input, TRACE);
        }
        assert!(KiroPreset.parse("not json", TRACE).is_err());
        assert!(KiroPreset.parse("[]", TRACE).is_err());
        // A write without cwd cannot be placed in a repository.
        let no_cwd = json!({"hook_event_name": "PreToolUse", "tool_name": "fs_write",
                            "tool_input": {"path": "a.rs"}})
        .to_string();
        assert!(KiroPreset.parse(&no_cwd, TRACE).is_err());
    }

    #[test]
    fn missing_session_id_still_records_the_edit() {
        let input = json!({"hook_event_name": "PreToolUse", "cwd": "/w", "tool_name": "fs_write",
                           "tool_input": {"path": "a.rs"}})
        .to_string();
        match &KiroPreset.parse(&input, TRACE).unwrap()[..] {
            [ParsedHookEvent::PreFileEdit(e)] => {
                assert_eq!(e.context.agent_id.id, UNKNOWN_SESSION);
            }
            other => panic!("expected PreFileEdit, got {other:?}"),
        }
    }
}
