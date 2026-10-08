//! Google Antigravity hook preset.
//!
//! Antigravity runs `hooks.json` command hooks around agent tool calls
//! (https://antigravity.google/docs/hooks). autter installs a `PreToolUse` and
//! a `PostToolUse` hook for the file-editing tools and `run_command` in the
//! shared global `~/.gemini/config/hooks.json`, which the docs list as the
//! global hooks file for Antigravity 2.0, the Antigravity CLI and the
//! Antigravity IDE.
//!
//! The stdin payload carries no event name (PreToolUse and PostToolUse
//! payloads differ only by an optional `error`), so each event gets its own
//! preset name: `antigravity-pre` and `antigravity-post`.
//!
//! Payload fields used (camelCase, per the docs' input contract):
//! `toolCall.name`, `toolCall.args`, `stepIdx`, `conversationId`,
//! `workspacePaths`, `modelName`. File-editing tools (`write_to_file`,
//! `replace_file_content`, `multi_replace_file_content`) name the edited file
//! in `toolCall.args.TargetFile`; `run_command` gives `CommandLine` and `Cwd`.
//!
//! The conversation transcript (`transcriptPath`, a `transcript.jsonl` under
//! the app data dir) has no documented record format, so it is not bridged
//! into prompt storage.

use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::AutterError;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Which hook event invoked the preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntigravityPhase {
    PreToolUse,
    PostToolUse,
}

pub struct AntigravityPreset {
    pub phase: AntigravityPhase,
}

/// Agent tool name recorded in attribution.
const TOOL: &str = "antigravity";

impl AgentPreset for AntigravityPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, AutterError> {
        let data: Value = serde_json::from_str(hook_input)
            .map_err(|e| AutterError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        // Older Antigravity CLI builds fired PostToolUse on non-tool steps
        // (fixed upstream); with no tool call there is nothing to record.
        let Some(tool_call) = data.get("toolCall").filter(|v| v.is_object()) else {
            return Ok(Vec::new());
        };
        let tool_name = tool_call.get("name").and_then(Value::as_str).unwrap_or("");
        let args = tool_call.get("args").cloned().unwrap_or(Value::Null);

        let class = bash_tool::classify_tool(Agent::Antigravity, tool_name);
        if class == ToolClass::Skip {
            return Ok(Vec::new());
        }

        let conversation_id = data
            .get("conversationId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AutterError::PresetError("conversationId not found".to_string()))?
            .to_string();
        let workspace_paths: Vec<PathBuf> = data
            .get("workspacePaths")
            .and_then(Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let model = data
            .get("modelName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string();
        // Pre and post payloads for one tool call share the step index.
        let tool_use_id = match data.get("stepIdx").and_then(Value::as_i64) {
            Some(step) => format!("{conversation_id}:{step}"),
            None => format!("{conversation_id}:{tool_name}"),
        };

        let file_paths = target_files(&args, &workspace_paths);
        let cwd = match class {
            ToolClass::Bash => args
                .get("Cwd")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .or_else(|| workspace_paths.first().cloned()),
            _ => file_paths
                .first()
                .and_then(|file| {
                    workspace_paths
                        .iter()
                        .find(|ws| file.starts_with(ws))
                        .cloned()
                        .or_else(|| file.parent().map(Path::to_path_buf))
                })
                .or_else(|| workspace_paths.first().cloned()),
        }
        .ok_or_else(|| {
            AutterError::PresetError("no workspace path or file path in hook input".to_string())
        })?;

        let mut metadata = HashMap::new();
        if let Some(path) = data.get("transcriptPath").and_then(Value::as_str) {
            // Kept for reference only: the transcript format is undocumented,
            // so it is deliberately not stored under `transcript_path` (which
            // would make the prompt bridge try to parse it).
            metadata.insert("antigravity_transcript_path".to_string(), path.to_string());
        }

        let context = PresetContext {
            agent_id: AgentId {
                tool: TOOL.to_string(),
                id: conversation_id.clone(),
                model,
            },
            external_session_id: conversation_id,
            trace_id: trace_id.to_string(),
            cwd,
            metadata,
        };

        let event = match (self.phase, class) {
            (AntigravityPhase::PreToolUse, ToolClass::Bash) => {
                ParsedHookEvent::PreBashCall(PreBashCall {
                    context,
                    tool_use_id,
                })
            }
            (AntigravityPhase::PostToolUse, ToolClass::Bash) => {
                ParsedHookEvent::PostBashCall(PostBashCall {
                    context,
                    tool_use_id,
                    stream_source: None,
                })
            }
            (AntigravityPhase::PreToolUse, _) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                dirty_files: None,
                tool_use_id: Some(tool_use_id),
            }),
            (AntigravityPhase::PostToolUse, _) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                dirty_files: None,
                stream_source: None,
                tool_use_id: Some(tool_use_id),
            }),
        };
        Ok(vec![event])
    }
}

/// `TargetFile` of a file-editing tool call, made absolute against the first
/// workspace path when the agent passed a relative path.
fn target_files(args: &Value, workspace_paths: &[PathBuf]) -> Vec<PathBuf> {
    let Some(target) = args
        .get("TargetFile")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };
    let path = PathBuf::from(target);
    if path.is_absolute() {
        vec![path]
    } else if let Some(ws) = workspace_paths.first() {
        vec![ws.join(path)]
    } else {
        vec![path]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(tool: &str, args: Value) -> String {
        // Shape taken from the PreToolUse/PostToolUse examples in
        // https://antigravity.google/docs/hooks.
        json!({
            "toolCall": {"name": tool, "args": args},
            "stepIdx": 19,
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "workspacePaths": ["/workspace/project"],
            "transcriptPath": "~/.gemini/antigravity-ide/brain/ec33ebf9/.system_generated/logs/transcript.jsonl",
            "artifactDirectoryPath": "~/.gemini/antigravity-ide/brain/ec33ebf9",
            "modelName": "gemini-3.6-flash-medium"
        })
        .to_string()
    }

    const PRE: AntigravityPreset = AntigravityPreset {
        phase: AntigravityPhase::PreToolUse,
    };
    const POST: AntigravityPreset = AntigravityPreset {
        phase: AntigravityPhase::PostToolUse,
    };

    #[test]
    fn file_edit_tools_map_to_pre_and_post_file_edits() {
        for tool in [
            "write_to_file",
            "replace_file_content",
            "multi_replace_file_content",
        ] {
            let input = payload(tool, json!({"TargetFile": "/workspace/project/src/lib.rs"}));
            match &PRE.parse(&input, "t_test123456789a").unwrap()[..] {
                [ParsedHookEvent::PreFileEdit(e)] => {
                    assert_eq!(
                        e.file_paths,
                        vec![PathBuf::from("/workspace/project/src/lib.rs")]
                    );
                    assert_eq!(e.context.cwd, PathBuf::from("/workspace/project"));
                    assert_eq!(e.context.agent_id.tool, "antigravity");
                    assert_eq!(e.context.agent_id.model, "gemini-3.6-flash-medium");
                    assert_eq!(
                        e.context.agent_id.id,
                        "ec33ebf9-0cba-4100-8142-c61503f6c587"
                    );
                    assert_eq!(
                        e.tool_use_id.as_deref(),
                        Some("ec33ebf9-0cba-4100-8142-c61503f6c587:19")
                    );
                }
                other => panic!("{tool}: expected PreFileEdit, got {other:?}"),
            }
            match &POST.parse(&input, "t_test123456789a").unwrap()[..] {
                [ParsedHookEvent::PostFileEdit(e)] => {
                    assert_eq!(
                        e.file_paths,
                        vec![PathBuf::from("/workspace/project/src/lib.rs")]
                    );
                    assert!(e.stream_source.is_none());
                    assert!(!e.context.metadata.contains_key("transcript_path"));
                }
                other => panic!("{tool}: expected PostFileEdit, got {other:?}"),
            }
        }
    }

    #[test]
    fn relative_target_file_resolves_against_workspace() {
        let input = payload("write_to_file", json!({"TargetFile": "src/new.rs"}));
        match &POST.parse(&input, "t_test123456789a").unwrap()[..] {
            [ParsedHookEvent::PostFileEdit(e)] => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/workspace/project/src/new.rs")]
                );
            }
            other => panic!("expected PostFileEdit, got {other:?}"),
        }
    }

    #[test]
    fn run_command_maps_to_bash_calls_using_its_cwd() {
        let input = payload(
            "run_command",
            json!({"CommandLine": "cargo fmt", "Cwd": "/workspace/project/sub", "WaitMsBeforeAsync": 5000}),
        );
        match &PRE.parse(&input, "t_test123456789a").unwrap()[..] {
            [ParsedHookEvent::PreBashCall(e)] => {
                assert_eq!(e.context.cwd, PathBuf::from("/workspace/project/sub"));
                assert_eq!(e.tool_use_id, "ec33ebf9-0cba-4100-8142-c61503f6c587:19");
            }
            other => panic!("expected PreBashCall, got {other:?}"),
        }
        assert!(matches!(
            &POST.parse(&input, "t_test123456789a").unwrap()[..],
            [ParsedHookEvent::PostBashCall(_)]
        ));
    }

    #[test]
    fn other_tools_and_non_tool_steps_are_ignored() {
        let input = payload(
            "view_file",
            json!({"AbsolutePath": "/workspace/project/a.rs"}),
        );
        assert!(PRE.parse(&input, "t_test123456789a").unwrap().is_empty());
        let no_tool = json!({
            "stepIdx": 3,
            "conversationId": "c",
            "workspacePaths": ["/workspace/project"]
        })
        .to_string();
        assert!(POST.parse(&no_tool, "t_test123456789a").unwrap().is_empty());
    }

    #[test]
    fn missing_conversation_id_is_an_error() {
        let input = json!({
            "toolCall": {"name": "write_to_file", "args": {"TargetFile": "/w/a.rs"}},
            "workspacePaths": ["/w"]
        })
        .to_string();
        assert!(PRE.parse(&input, "t_test123456789a").is_err());
    }
}
