//! Bridge AI prompt transcripts into the CAS (content-addressed storage) queue.
//!
//! Each AI checkpoint records the path to the agent's transcript file in its
//! `agent_metadata` (`"transcript_path"`). At post-commit time we read that
//! transcript, redact secrets, and enqueue it as a CAS object keyed by the hash
//! of its canonicalized content. The resulting `cas:<hash>` reference is written
//! onto the matching [`PromptRecord::messages_url`] / [`SessionRecord::messages_url`]
//! so the authorship note can point back at the full conversation that produced
//! each AI attribution.
//!
//! The durable queue (`cas_sync_queue` in the internal DB) is drained and
//! uploaded by the daemon's telemetry flush loop.
//!
//! This is best-effort: any failure for a single session is swallowed so that
//! note generation is never blocked by transcript handling.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::api::client::{ApiClient, ApiContext};
use crate::api::types::CasMessagesObject;
use crate::authorship::authorship_log::{PromptRecord, SessionRecord};
use crate::authorship::authorship_log_serialization::{generate_session_id, generate_short_hash};
use crate::authorship::internal_db::InternalDatabase;
use crate::authorship::transcript::Message;
use crate::authorship::working_log::{AgentId, Checkpoint};
use crate::config;
use crate::error::AutterError;

/// Schema tag stored in each CAS transcript object's metadata so consumers
/// (PR review, blame, etc.) can evolve the shape over time.
const TRANSCRIPT_SCHEMA: &str = "cas/transcript/1.0.0";

/// For every AI prompt/session that has a captured transcript, enqueue the
/// transcript as a CAS object and record the `cas:<hash>` on `messages_url`.
///
/// `checkpoints` is the working log for the commit being processed; it is the
/// only place the transcript file path is available (via `agent_metadata`).
pub fn enqueue_prompt_transcripts(
    prompts: &mut BTreeMap<String, PromptRecord>,
    sessions: &mut BTreeMap<String, SessionRecord>,
    checkpoints: &[Checkpoint],
) {
    if prompts.is_empty() && sessions.is_empty() {
        return;
    }

    let transcript_paths = transcript_paths_from_checkpoints(checkpoints);
    if transcript_paths.is_empty() {
        return;
    }

    for (hash, record) in prompts.iter_mut() {
        if record.messages_url.is_some() {
            continue;
        }
        let Some(path) = transcript_paths.get(hash) else {
            continue;
        };
        if let Ok(Some(cas_hash)) = enqueue_transcript_file(path, &record.agent_id) {
            record.messages_url = Some(format!("cas:{cas_hash}"));
        }
    }

    for (session_key, record) in sessions.iter_mut() {
        if record.messages_url.is_some() {
            continue;
        }
        let Some(path) = transcript_paths.get(session_key) else {
            continue;
        };
        if let Ok(Some(cas_hash)) = enqueue_transcript_file(path, &record.agent_id) {
            record.messages_url = Some(format!("cas:{cas_hash}"));
        }
    }
}

/// Build a map of prompt/session hash -> transcript path from checkpoint metadata.
fn transcript_paths_from_checkpoints(checkpoints: &[Checkpoint]) -> HashMap<String, String> {
    let mut transcript_paths: HashMap<String, String> = HashMap::new();
    for cp in checkpoints {
        let (Some(agent_id), Some(meta)) = (&cp.agent_id, &cp.agent_metadata) else {
            continue;
        };
        if let Some(path) = meta.get("transcript_path") {
            let short_hash = generate_short_hash(&agent_id.id, &agent_id.tool);
            let session_id = generate_session_id(&agent_id.id, &agent_id.tool);
            transcript_paths.insert(short_hash, path.clone());
            transcript_paths.insert(session_id, path.clone());
        }
    }
    transcript_paths
}

/// Read and normalize a single transcript file, then enqueue it as a CAS object.
/// Returns the canonical content hash on success, or `None` when there is
/// nothing worth storing (file gone, empty, or unparseable).
fn enqueue_transcript_file(path: &str, agent_id: &AgentId) -> Result<Option<String>, AutterError> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        // Transcript file no longer present (e.g. cleaned up by the agent).
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }

    let events = parse_transcript_events(&raw);
    if events.is_empty() {
        return Ok(None);
    }

    // Normalize each agent's raw event format into typed transcript messages.
    let messages: Vec<Message> = events.iter().flat_map(messages_from_event).collect();
    if messages.is_empty() {
        return Ok(None);
    }

    let cas_object = CasMessagesObject { messages: messages.clone() };
    let mut payload = serde_json::to_value(&cas_object)
        .map_err(|e| AutterError::Generic(format!("Failed to serialize transcript: {e}")))?;

    // Redact secrets before the content ever leaves the local machine, matching
    // the redaction applied to streamed session events.
    payload = crate::daemon::transcript_redaction::redact_json_secrets(payload);

    let mut metadata: HashMap<String, String> = HashMap::new();
    metadata.insert("schema".to_string(), TRANSCRIPT_SCHEMA.to_string());
    metadata.insert("kind".to_string(), "transcript".to_string());
    metadata.insert("tool".to_string(), agent_id.tool.clone());
    metadata.insert("model".to_string(), agent_id.model.clone());
    metadata.insert("session_id".to_string(), agent_id.id.clone());

    let db = InternalDatabase::global()?;
    let mut db_lock = db
        .lock()
        .map_err(|_| AutterError::Generic("CAS internal DB lock poisoned".to_string()))?;
    let cas_hash = db_lock.enqueue_cas_object(&payload, Some(&metadata))?;

    // Cache locally so show-prompt can resolve before cloud upload completes.
    if let Ok(messages_json) = serde_json::to_string(&messages) {
        let _ = db_lock.set_cas_cache(&cas_hash, &messages_json);
    }

    Ok(Some(cas_hash))
}

/// Resolve transcript messages from a `cas:<hash>` URL.
///
/// Lookup order:
/// 1. Local SQLite cache (`cas_cache`)
/// 2. Local sync queue (`cas_sync_queue`, not yet uploaded)
/// 3. Cloud CAS API (when authenticated)
pub fn resolve_cas_messages(messages_url: &str) -> Result<Option<Vec<Message>>, AutterError> {
    let Some(hash) = messages_url.strip_prefix("cas:") else {
        return Ok(None);
    };
    if hash.is_empty() {
        return Ok(None);
    }

    if let Ok(db) = InternalDatabase::global()
        && let Ok(db_lock) = db.lock()
    {
        if let Some(cached) = db_lock.get_cas_cache(hash)?
            && let Ok(messages) = serde_json::from_str::<Vec<Message>>(&cached)
        {
            return Ok(Some(messages));
        }

        if let Some(queue_data) = db_lock.get_cas_queue_data(hash)?
            && let Ok(messages) = extract_messages_from_cas_payload(&queue_data)
        {
            return Ok(Some(messages));
        }
    }

    let cfg = config::Config::fresh();
    let dataplane_url = if cfg.notes_backend_kind() == config::NotesBackendKind::Http {
        cfg.notes_backend_url().map(|s| s.to_string())
    } else {
        None
    };
    let client = ApiClient::new(ApiContext::new(dataplane_url));
    if !client.is_logged_in() && !client.has_api_key() {
        return Ok(None);
    }

    let response = client.read_ca_prompt_store(&[hash])?;
    for result in response.results {
        if result.status == "ok"
            && result.hash == hash
            && let Some(content) = result.content
            && let Ok(messages) = extract_messages_from_cas_value(&content)
        {
            if let Ok(db) = InternalDatabase::global()
                && let Ok(mut db_lock) = db.lock()
                && let Ok(messages_json) = serde_json::to_string(&messages)
            {
                let _ = db_lock.set_cas_cache(hash, &messages_json);
            }
            return Ok(Some(messages));
        }
    }

    Ok(None)
}

fn extract_messages_from_cas_payload(payload: &str) -> Result<Vec<Message>, AutterError> {
    let value: Value = serde_json::from_str(payload)
        .map_err(|e| AutterError::Generic(format!("Failed to parse CAS payload: {e}")))?;
    extract_messages_from_cas_value(&value)
}

fn extract_messages_from_cas_value(value: &Value) -> Result<Vec<Message>, AutterError> {
    if let Ok(obj) = serde_json::from_value::<CasMessagesObject>(value.clone()) {
        return Ok(obj.messages);
    }
    if let Some(messages) = value.get("messages") {
        return serde_json::from_value(messages.clone())
            .map_err(|e| AutterError::Generic(format!("Failed to parse CAS messages: {e}")));
    }
    Err(AutterError::Generic(
        "CAS payload missing messages".to_string(),
    ))
}

/// Convert one raw transcript event into zero or more typed [`Message`]s.
fn messages_from_event(event: &Value) -> Vec<Message> {
    let (event, outer_timestamp) = normalize_transcript_event(event);
    let role = event_role(&event);
    let timestamp = event
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(outer_timestamp);

    if event
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t == "function_call")
    {
        let name = event
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let input = event
            .get("arguments")
            .and_then(|v| {
                if let Some(raw) = v.as_str() {
                    serde_json::from_str(raw).ok()
                } else {
                    Some(v.clone())
                }
            })
            .unwrap_or(Value::Null);
        return vec![Message::ToolUse {
            name,
            input,
            timestamp,
        }];
    }

    // Content is nested under `message` (Claude) or at the top level (Gemini/Codex).
    let content = event
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| event.get("content"));

    match content {
        Some(Value::String(text)) => text_message(role, text.clone(), timestamp)
            .into_iter()
            .collect(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .flat_map(|block| block_to_messages(role, block, &timestamp))
            .collect(),
        _ => Vec::new(),
    }
}

/// Normalize agent-specific transcript envelopes into a common event shape.
fn normalize_transcript_event(event: &Value) -> (Value, Option<String>) {
    let outer_timestamp = event
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);

    if event.get("type").and_then(Value::as_str) == Some("response_item")
        && let Some(payload) = event.get("payload")
    {
        return (payload.clone(), outer_timestamp);
    }

    (event.clone(), None)
}

/// Map a single content block to messages, preserving thinking and tool calls.
fn block_to_messages(role: Role, block: &Value, timestamp: &Option<String>) -> Vec<Message> {
    // Bare string block (some formats use `["text", ...]`).
    if let Value::String(text) = block {
        return text_message(role, text.clone(), timestamp.clone())
            .into_iter()
            .collect();
    }

    match block.get("type").and_then(Value::as_str) {
        Some("tool_use") => {
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            vec![Message::ToolUse {
                name,
                input,
                timestamp: timestamp.clone(),
            }]
        }
        Some("thinking") => {
            let text = block
                .get("thinking")
                .or_else(|| block.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if text.is_empty() {
                vec![]
            } else {
                vec![Message::Thinking {
                    text,
                    timestamp: timestamp.clone(),
                }]
            }
        }
        // "text" blocks and Gemini/Codex `{text}` / `{input_text}` / `{output_text}` blocks.
        _ => block
            .get("text")
            .or_else(|| block.get("input_text"))
            .or_else(|| block.get("output_text"))
            .and_then(Value::as_str)
            .and_then(|text| text_message(role, text.to_string(), timestamp.clone()))
            .into_iter()
            .collect(),
    }
}

/// Role of a transcript event, normalized to user vs assistant.
#[derive(Clone, Copy)]
enum Role {
    User,
    Assistant,
}

fn event_role(event: &Value) -> Role {
    // Codex uses type="message" with a separate role field.
    if let Some(role) = event.get("role").and_then(Value::as_str) {
        if role.eq_ignore_ascii_case("user") {
            return Role::User;
        }
        if role.eq_ignore_ascii_case("assistant") {
            return Role::Assistant;
        }
    }

    let raw = event
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    if raw.eq_ignore_ascii_case("user") {
        Role::User
    } else {
        Role::Assistant
    }
}

/// Build a user/assistant text message, dropping empty text.
fn text_message(role: Role, text: String, timestamp: Option<String>) -> Option<Message> {
    if text.trim().is_empty() {
        return None;
    }
    Some(match role {
        Role::User => Message::User { text, timestamp },
        Role::Assistant => Message::Assistant { text, timestamp },
    })
}

/// Transcripts come in two shapes: a single JSON document (array or object),
/// or JSONL with one JSON value per line. Normalize both into a flat list of
/// event values, skipping any unparseable lines.
fn parse_transcript_events(raw: &str) -> Vec<serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Array(arr)) => arr,
        Ok(other) => vec![other],
        Err(_) => raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonl_transcript() {
        let raw = "{\"role\":\"user\"}\n{\"role\":\"assistant\"}\n";
        let events = parse_transcript_events(raw);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn parses_single_json_array_transcript() {
        let raw = "[{\"role\":\"user\"},{\"role\":\"assistant\"}]";
        let events = parse_transcript_events(raw);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn parses_single_json_object_transcript() {
        let raw = "{\"messages\":[]}";
        let events = parse_transcript_events(raw);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn skips_unparseable_jsonl_lines() {
        let raw = "{\"ok\":1}\nnot json\n{\"ok\":2}";
        let events = parse_transcript_events(raw);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn claude_user_string_content_maps_to_user_message() {
        let event = serde_json::json!({
            "type": "user",
            "message": { "content": "Write a hello world function" },
            "timestamp": "2025-01-01T12:00:00Z"
        });
        let msgs = messages_from_event(&event);
        assert_eq!(
            msgs,
            vec![Message::User {
                text: "Write a hello world function".to_string(),
                timestamp: Some("2025-01-01T12:00:00Z".to_string()),
            }]
        );
    }

    #[test]
    fn claude_assistant_blocks_map_text_and_tool_use() {
        let event = serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "text", "text": "I'll create it." },
                { "type": "tool_use", "name": "Write", "input": { "file_path": "hello.py" } }
            ], "model": "claude-sonnet-4" },
            "timestamp": "2025-01-01T12:00:02Z"
        });
        let msgs = messages_from_event(&event);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(&msgs[0], Message::Assistant { text, .. } if text == "I'll create it."));
        assert!(matches!(&msgs[1], Message::ToolUse { name, .. } if name == "Write"));
    }

    #[test]
    fn gemini_top_level_content_array_maps() {
        let event = serde_json::json!({
            "type": "user",
            "content": [{ "text": "Hello" }]
        });
        let msgs = messages_from_event(&event);
        assert_eq!(
            msgs,
            vec![Message::User {
                text: "Hello".to_string(),
                timestamp: None,
            }]
        );
    }

    #[test]
    fn gemini_assistant_role_maps_to_assistant() {
        let event = serde_json::json!({
            "type": "gemini",
            "content": "Hi there",
            "model": "gemini-3-flash-preview"
        });
        let msgs = messages_from_event(&event);
        assert!(matches!(&msgs[0], Message::Assistant { text, .. } if text == "Hi there"));
    }

    #[test]
    fn thinking_block_maps_to_thinking_message() {
        let event = serde_json::json!({
            "type": "assistant",
            "message": { "content": [{ "type": "thinking", "thinking": "Let me reason." }] }
        });
        let msgs = messages_from_event(&event);
        assert!(matches!(&msgs[0], Message::Thinking { text, .. } if text == "Let me reason."));
    }

    #[test]
    fn codex_response_item_user_message_maps() {
        let event = serde_json::json!({
            "timestamp": "2026-02-11T05:53:33.360Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "Refactor src/main.rs" }]
            }
        });
        let msgs = messages_from_event(&event);
        assert_eq!(
            msgs,
            vec![Message::User {
                text: "Refactor src/main.rs".to_string(),
                timestamp: Some("2026-02-11T05:53:33.360Z".to_string()),
            }]
        );
    }

    #[test]
    fn codex_response_item_function_call_maps_to_tool_use() {
        let event = serde_json::json!({
            "timestamp": "2026-02-11T05:53:33.420Z",
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "apply_patch",
                "arguments": "{\"patch\":\"*** Begin Patch\"}"
            }
        });
        let msgs = messages_from_event(&event);
        assert!(matches!(&msgs[0], Message::ToolUse { name, .. } if name == "apply_patch"));
    }

    #[test]
    fn codex_response_item_assistant_message_maps() {
        let event = serde_json::json!({
            "timestamp": "2026-02-11T05:53:33.520Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "Done." }]
            }
        });
        let msgs = messages_from_event(&event);
        assert!(matches!(&msgs[0], Message::Assistant { text, .. } if text == "Done."));
    }

    #[test]
    fn event_without_content_yields_nothing() {
        let event = serde_json::json!({ "type": "system", "uuid": "x" });
        assert!(messages_from_event(&event).is_empty());
    }
}
