pub mod parse;

mod agent_v1;
mod ai_tab;
mod amp;
mod antigravity;
pub(crate) mod claude;
pub(crate) mod codex;
mod continue_cli;
pub(crate) mod cursor;
pub(crate) mod droid;
mod firebender;
pub(crate) mod gemini;
pub(crate) mod github_copilot;
mod human;
mod known_human;
mod mock_ai;
mod mock_known_human;
mod opencode;
mod pi;
pub(crate) mod windsurf;

use crate::authorship::working_log::AgentId;
use crate::error::AutterError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetContext {
    pub agent_id: AgentId,
    pub external_session_id: String,
    pub trace_id: String,
    pub cwd: PathBuf,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ParsedHookEvent {
    PreFileEdit(PreFileEdit),
    PostFileEdit(PostFileEdit),
    PreBashCall(PreBashCall),
    PostBashCall(PostBashCall),
    KnownHumanEdit(KnownHumanEdit),
    UntrackedEdit(UntrackedEdit),
    /// The agent's transcript changed without a file edit: the person
    /// submitted a prompt, or the agent finished its turn. No checkpoint is
    /// taken; the daemon's stream worker is told to read the transcript now
    /// instead of waiting for its next sweep, so sessions that never edit a
    /// file (questions, reviews, planning) reach the platform within seconds.
    SessionActivity(SessionActivity),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreFileEdit {
    pub context: PresetContext,
    pub file_paths: Vec<PathBuf>,
    pub dirty_files: Option<HashMap<PathBuf, String>>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostFileEdit {
    pub context: PresetContext,
    pub file_paths: Vec<PathBuf>,
    pub dirty_files: Option<HashMap<PathBuf, String>>,
    pub stream_source: Option<StreamSource>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownHumanEdit {
    pub trace_id: String,
    pub cwd: PathBuf,
    pub file_paths: Vec<PathBuf>,
    pub dirty_files: Option<HashMap<PathBuf, String>>,
    pub editor_metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UntrackedEdit {
    pub trace_id: String,
    pub cwd: PathBuf,
    pub file_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreBashCall {
    pub context: PresetContext,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostBashCall {
    pub context: PresetContext,
    pub tool_use_id: String,
    pub stream_source: Option<StreamSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionActivity {
    pub context: PresetContext,
    pub stream_source: StreamSource,
}

/// A prompt was submitted or the agent ended its turn: tell the daemon to read
/// the session's transcript now. Every agent with such a hook event maps it
/// here. Without a transcript there is nothing to read, so the hook is skipped.
pub(crate) fn session_activity_event(
    context: PresetContext,
    stream_source: Option<StreamSource>,
) -> Result<Vec<ParsedHookEvent>, AutterError> {
    match stream_source {
        Some(stream_source) => Ok(vec![ParsedHookEvent::SessionActivity(SessionActivity {
            context,
            stream_source,
        })]),
        None => Err(AutterError::PresetError(
            "Skipping session activity: no transcript for this session.".to_string(),
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSource {
    pub path: PathBuf,
    pub format: StreamFormat,
    /// Session ID for this transcript (used to query/create session in DB).
    pub session_id: String,
    /// External thread/conversation ID (agent-specific identifier).
    pub external_session_id: String,
    /// Parent session ID for subagent transcripts.
    #[serde(default)]
    pub external_parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamFormat {
    ClaudeJsonl,
    ContinueJson,
    GeminiJsonl,
    WindsurfJsonl,
    CodexJsonl,
    CursorJsonl,
    DroidJsonl,
    CopilotSessionJson,
    CopilotEventStreamJsonl,
    AmpThreadJson,
    OpenCodeSqlite,
    PiJsonl,
    FirebenderJsonl,
    AntigravityJsonl,
}

impl StreamFormat {
    pub fn watermark_type(self) -> crate::streams::watermark::WatermarkType {
        use crate::streams::watermark::WatermarkType;
        match self {
            Self::ClaudeJsonl
            | Self::CursorJsonl
            | Self::GeminiJsonl
            | Self::WindsurfJsonl
            | Self::CodexJsonl
            | Self::PiJsonl
            | Self::FirebenderJsonl
            | Self::AntigravityJsonl
            | Self::CopilotEventStreamJsonl => WatermarkType::ByteOffset,
            Self::DroidJsonl => WatermarkType::Hybrid,
            Self::CopilotSessionJson | Self::ContinueJson | Self::AmpThreadJson => {
                WatermarkType::RecordIndex
            }
            Self::OpenCodeSqlite => WatermarkType::Timestamp,
        }
    }
}

pub trait AgentPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, AutterError>;
}

pub fn resolve_preset(name: &str) -> Result<Box<dyn AgentPreset>, AutterError> {
    match name {
        "claude" => Ok(Box::new(claude::ClaudePreset)),
        "codex" => Ok(Box::new(codex::CodexPreset)),
        "gemini" => Ok(Box::new(gemini::GeminiPreset)),
        "windsurf" => Ok(Box::new(windsurf::WindsurfPreset)),
        "continue-cli" => Ok(Box::new(continue_cli::ContinueCliPreset)),
        "cursor" => Ok(Box::new(cursor::CursorPreset)),
        "cursor-background" => Ok(Box::new(cursor::CursorBackgroundPreset)),
        "github-copilot" => Ok(Box::new(github_copilot::GithubCopilotPreset)),
        "amp" => Ok(Box::new(amp::AmpPreset)),
        "ai_tab" => Ok(Box::new(ai_tab::AiTabPreset)),
        "firebender" => Ok(Box::new(firebender::FirebenderPreset)),
        "agent-v1" => Ok(Box::new(agent_v1::AgentV1Preset)),
        "droid" => Ok(Box::new(droid::DroidPreset)),
        "opencode" => Ok(Box::new(opencode::OpenCodePreset)),
        "pi" => Ok(Box::new(pi::PiPreset)),
        "antigravity-pre" => Ok(Box::new(antigravity::AntigravityPreset {
            phase: antigravity::AntigravityPhase::PreToolUse,
        })),
        "antigravity-post" => Ok(Box::new(antigravity::AntigravityPreset {
            phase: antigravity::AntigravityPhase::PostToolUse,
        })),
        "human" => Ok(Box::new(human::HumanPreset)),
        "mock_ai" => Ok(Box::new(mock_ai::MockAiPreset)),
        "known_human" => Ok(Box::new(known_human::KnownHumanPreset)),
        "mock_known_human" => Ok(Box::new(mock_known_human::MockKnownHumanPreset)),
        _ => Err(AutterError::PresetError(format!(
            "Unknown preset: {}",
            name
        ))),
    }
}

#[cfg(test)]
mod session_activity_preset_tests {
    use super::*;
    use serde_json::json;

    fn activity(preset: &str, input: serde_json::Value) -> SessionActivity {
        let events = resolve_preset(preset)
            .unwrap()
            .parse(&input.to_string(), "t_test123456789a")
            .unwrap_or_else(|e| panic!("{preset}: {e}"));
        assert_eq!(events.len(), 1, "{preset}");
        match events.into_iter().next().unwrap() {
            ParsedHookEvent::SessionActivity(e) => e,
            other => panic!("{preset}: expected SessionActivity, got {other:?}"),
        }
    }

    #[test]
    fn codex_prompt_and_stop() {
        for event in ["UserPromptSubmit", "Stop", "SubagentStop"] {
            let e = activity(
                "codex",
                json!({"cwd": "/p", "session_id": "c1", "hook_event_name": event,
                       "transcript_path": "/h/.codex/sessions/rollout-c1.jsonl", "prompt": "hi"}),
            );
            assert_eq!(e.stream_source.format, StreamFormat::CodexJsonl, "{event}");
            assert_eq!(e.context.agent_id.tool, "codex");
        }
    }

    #[test]
    fn droid_prompt_and_stop() {
        for event in ["UserPromptSubmit", "Stop", "SubagentStop"] {
            let e = activity(
                "droid",
                json!({"cwd": "/p", "session_id": "d1", "hookEventName": event,
                       "transcript_path": "/h/.factory/sessions/d1.jsonl"}),
            );
            assert_eq!(e.stream_source.format, StreamFormat::DroidJsonl, "{event}");
        }
    }

    #[test]
    fn gemini_before_and_after_agent() {
        for event in ["BeforeAgent", "AfterAgent"] {
            let e = activity(
                "gemini",
                json!({"cwd": "/p", "session_id": "g1", "hook_event_name": event,
                       "transcript_path": "/h/.gemini/tmp/g1.json", "prompt": "hi"}),
            );
            assert_eq!(e.stream_source.format, StreamFormat::GeminiJsonl, "{event}");
            assert_eq!(
                e.context.agent_id.model, "unknown",
                "no transcript read on the prompt path"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn cursor_stop_with_and_without_transcript() {
        let e = activity(
            "cursor",
            json!({"conversation_id": "k1", "generation_id": "g", "hook_event_name": "stop",
                   "status": "completed", "workspace_roots": ["/p"],
                   "transcript_path": "/h/.cursor/projects/p/agent-transcripts/k1.jsonl"}),
        );
        assert_eq!(e.stream_source.format, StreamFormat::CursorJsonl);
        assert_eq!(e.context.cwd, std::path::PathBuf::from("/p"));

        // Without a transcript and with no conversation in Cursor's database,
        // there is nothing to read.
        let tmp = tempfile::tempdir().unwrap();
        let _env = CursorStateEnv::set(&tmp.path().join("missing.vscdb"), &tmp.path().join("out"));
        let no_transcript = json!({"conversation_id": "k1", "hook_event_name": "stop",
                                   "workspace_roots": ["/p"], "transcript_path": null});
        assert!(
            resolve_preset("cursor")
                .unwrap()
                .parse(&no_transcript.to_string(), "t")
                .is_err(),
            "nothing to read without a transcript"
        );
    }

    /// Points the Cursor database fallback at test paths; restores on drop.
    /// Tests using it are `#[serial]` because the environment is process-wide.
    struct CursorStateEnv;

    impl CursorStateEnv {
        fn set(db: &std::path::Path, out: &std::path::Path) -> Self {
            unsafe {
                std::env::set_var("CURSOR_STATE_DB", db);
                std::env::set_var("AUTTER_CURSOR_EXPORT_DIR", out);
            }
            Self
        }
    }

    impl Drop for CursorStateEnv {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("CURSOR_STATE_DB");
                std::env::remove_var("AUTTER_CURSOR_EXPORT_DIR");
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn cursor_stop_without_transcript_reads_cursors_database() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state.vscdb");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
        )
        .unwrap();
        for (key, value) in [
            (
                "composerData:k2",
                json!({"fullConversationHeadersOnly": [{"bubbleId": "b1", "type": 1}]}),
            ),
            (
                "bubbleId:k2:b1",
                json!({"type": 1, "text": "Rename the retry flag"}),
            ),
        ] {
            conn.execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, value.to_string()],
            )
            .unwrap();
        }
        drop(conn);
        let out = tmp.path().join("out");
        let _env = CursorStateEnv::set(&db, &out);

        let e = activity(
            "cursor",
            json!({"conversation_id": "k2", "hook_event_name": "stop",
                   "workspace_roots": ["/p"], "transcript_path": null}),
        );
        assert_eq!(e.stream_source.format, StreamFormat::CursorJsonl);
        assert_eq!(e.stream_source.path, out.join("k2.jsonl"));
        assert_eq!(e.stream_source.external_session_id, "k2");
        let written = std::fs::read_to_string(out.join("k2.jsonl")).unwrap();
        assert!(written.contains("Rename the retry flag"));
        assert!(written.contains(r#""role":"user""#));
    }

    #[test]
    fn firebender_stop_and_transcripts() {
        let e = activity(
            "firebender",
            json!({"hook_event_name": "stop", "conversation_id": "fb-1", "generation_id": "gen-9",
                   "model": "claude-sonnet-4-5", "workspace_roots": ["/p"],
                   "transcript_path": "/h/.firebender/projects/p/transcripts/agent-fb-1.jsonl"}),
        );
        assert_eq!(e.stream_source.format, StreamFormat::FirebenderJsonl);
        assert_eq!(e.stream_source.external_session_id, "fb-1");
        assert_eq!(e.context.agent_id.id, "firebender-fb-1");
        assert_eq!(e.context.cwd, std::path::PathBuf::from("/p"));

        // A transcript path without a conversation id still names the session.
        let e = activity(
            "firebender",
            json!({"hook_event_name": "sessionEnd", "workspace_roots": ["/p"],
                   "transcript_path": "/h/.firebender/projects/p/transcripts/agent-fb-2.jsonl"}),
        );
        assert_eq!(e.stream_source.external_session_id, "fb-2");

        // Transcripts off: nothing to read.
        assert!(
            resolve_preset("firebender")
                .unwrap()
                .parse(
                    &json!({"hook_event_name": "stop", "conversation_id": "fb-1",
                            "workspace_roots": ["/p"], "transcript_path": null})
                    .to_string(),
                    "t"
                )
                .is_err()
        );

        // Edits in the same conversation carry the transcript too.
        let events = resolve_preset("firebender")
            .unwrap()
            .parse(
                &json!({"hook_event_name": "postToolUse", "conversation_id": "fb-1",
                        "model": "m", "repo_working_dir": "/p", "tool_name": "Write",
                        "tool_input": {"file_path": "src/lib.rs"},
                        "transcript_path": "/h/.firebender/projects/p/transcripts/agent-fb-1.jsonl"})
                .to_string(),
                "t",
            )
            .unwrap();
        match &events[..] {
            [ParsedHookEvent::PostFileEdit(e)] => {
                assert_eq!(e.context.agent_id.id, "firebender-fb-1");
                assert_eq!(
                    e.stream_source.as_ref().map(|s| s.format),
                    Some(StreamFormat::FirebenderJsonl)
                );
            }
            other => panic!("expected PostFileEdit, got {other:?}"),
        }
    }

    #[test]
    fn windsurf_cascade_response() {
        for event in [
            "post_cascade_response_with_transcript",
            "post_cascade_response",
            "pre_user_prompt",
        ] {
            let e = activity(
                "windsurf",
                json!({"trajectory_id": "w1", "agent_action_name": event, "cwd": "/p",
                       "tool_info": {"transcript_path": "/h/.windsurf/transcripts/w1.jsonl"}}),
            );
            assert_eq!(
                e.stream_source.format,
                StreamFormat::WindsurfJsonl,
                "{event}"
            );
        }
    }

    #[test]
    fn copilot_vscode_and_cli_and_claude_transcripts() {
        let vscode = activity(
            "github-copilot",
            json!({"hook_event_name": "Stop", "session_id": "v1", "cwd": "/p",
                   "transcript_path": "/h/Code/User/workspaceStorage/ab/GitHub.copilot-chat/transcripts/v1.jsonl"}),
        );
        assert_eq!(vscode.context.agent_id.tool, "github-copilot");
        assert_eq!(
            vscode.stream_source.format,
            StreamFormat::CopilotEventStreamJsonl
        );

        let cli = activity(
            "github-copilot",
            json!({"hookEventName": "Stop", "sessionId": "s1", "cwd": "/p",
                   "transcriptPath": "/h/.copilot/session-state/s1/events.jsonl"}),
        );
        assert_eq!(cli.context.agent_id.tool, "github-copilot-cli");
        assert_eq!(
            cli.stream_source.format,
            StreamFormat::CopilotEventStreamJsonl
        );

        let claude = json!({"hook_event_name": "UserPromptSubmit", "session_id": "c", "cwd": "/p",
                            "transcript_path": "/h/.claude/projects/p/c.jsonl"});
        assert!(
            resolve_preset("github-copilot")
                .unwrap()
                .parse(&claude.to_string(), "t")
                .is_err(),
            "a Claude session is Claude's preset's to report"
        );
    }

    #[test]
    fn amp_turn_end() {
        // Amp reads only a thread file that exists.
        let dir = tempfile::tempdir().unwrap();
        let thread = dir.path().join("T-1.json");
        std::fs::write(&thread, "{\"id\":\"T-1\",\"messages\":[]}").unwrap();
        let e = activity(
            "amp",
            json!({"hook_event_name": "Stop", "thread_id": "T-1", "cwd": "/p",
                   "transcript_path": thread.to_string_lossy()}),
        );
        assert_eq!(e.stream_source.format, StreamFormat::AmpThreadJson);
        assert_eq!(e.context.external_session_id, "T-1");
    }

    #[test]
    fn pi_agent_end() {
        let e = activity(
            "pi",
            json!({"hook_event_name": "agent_end", "session_id": "p1",
                   "session_path": "/h/.pi/agent/sessions/p1.jsonl", "cwd": "/p", "model": "anthropic/claude-sonnet-5"}),
        );
        assert_eq!(e.stream_source.format, StreamFormat::PiJsonl);
        assert_eq!(e.context.agent_id.model, "claude-sonnet-5");
    }

    #[test]
    fn opencode_idle_never_takes_a_checkpoint() {
        // The session's SQLite store decides whether there is a transcript; either
        // way a turn end must not become a file checkpoint.
        let input = json!({"hook_event_name": "Stop", "session_id": "o1", "cwd": "/p"});
        match resolve_preset("opencode")
            .unwrap()
            .parse(&input.to_string(), "t")
        {
            Ok(events) => assert!(matches!(events[0], ParsedHookEvent::SessionActivity(_))),
            Err(e) => assert!(e.to_string().contains("no transcript"), "{e}"),
        }
    }
}
