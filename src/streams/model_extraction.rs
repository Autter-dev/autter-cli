use crate::streams::sweep::StreamFormat;
use crate::streams::types::StreamError;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

pub fn extract_model(
    path: &Path,
    format: StreamFormat,
    session_id: Option<&str>,
) -> Result<Option<String>, StreamError> {
    match format {
        StreamFormat::ClaudeJsonl
        | StreamFormat::CopilotEventStreamJsonl
        | StreamFormat::GeminiJsonl
        | StreamFormat::CursorJsonl
        | StreamFormat::FirebenderJsonl => extract_model_from_jsonl_tail(path),
        StreamFormat::CopilotSessionJson => extract_model_from_copilot_session_json(path),
        StreamFormat::AmpThreadJson => extract_model_from_amp_thread_json(path),
        StreamFormat::OpenCodeSqlite => extract_model_from_opencode_sqlite(path, session_id),
        // Droid uses extract_model_from_droid_settings() with the settings path instead
        _ => Ok(None),
    }
}

pub fn extract_model_from_droid_settings(
    settings_path: &Path,
) -> Result<Option<String>, StreamError> {
    let content = match std::fs::read_to_string(settings_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(None),
        Err(_) => return Ok(None),
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    Ok(json.get("model").and_then(|v| v.as_str()).map(String::from))
}

/// How far back from the end of a JSONL transcript the model search reads.
/// Transcripts can hold single records of a megabyte or more (a Write tool's
/// input, an Edit result with the whole original file), so the search reads
/// whole lines backwards rather than a fixed tail window, but stops somewhere.
const MAX_REVERSE_SCAN_BYTES: u64 = 64 * 1024 * 1024;

/// Once the newest model is known, how much further back the search keeps
/// looking for the assistant message that issued a given tool call.
const TOOL_USE_SEARCH_BYTES: u64 = 16 * 1024 * 1024;

/// How much of the start of a transcript the head fallback reads.
const MAX_HEAD_SCAN_BYTES: u64 = 8 * 1024 * 1024;

const REVERSE_CHUNK_BYTES: u64 = 256 * 1024;

/// Reads a file's lines from the end toward the start, a whole line at a
/// time however long it is.
struct ReverseLines {
    file: File,
    /// Bytes before this offset have not been read yet.
    pos: u64,
    /// Unconsumed bytes: a line whose start may lie before `pos`, followed by
    /// complete lines.
    buf: Vec<u8>,
    scanned: u64,
}

impl ReverseLines {
    fn new(file: File) -> Option<Self> {
        let len = file.metadata().ok()?.len();
        Some(Self {
            file,
            pos: len,
            buf: Vec::new(),
            scanned: 0,
        })
    }

    fn reached_start(&self) -> bool {
        self.pos == 0
    }

    fn next_line(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(idx) = self.buf.iter().rposition(|&b| b == b'\n') {
                let line = self.buf.split_off(idx + 1);
                self.buf.truncate(idx);
                if line.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                return Some(line);
            }
            if self.pos == 0 {
                if self.buf.iter().all(|b| b.is_ascii_whitespace()) {
                    self.buf.clear();
                    return None;
                }
                return Some(std::mem::take(&mut self.buf));
            }
            let chunk = std::cmp::min(REVERSE_CHUNK_BYTES, self.pos);
            self.pos -= chunk;
            let mut bytes = vec![0u8; chunk as usize];
            self.file.seek(SeekFrom::Start(self.pos)).ok()?;
            self.file.read_exact(&mut bytes).ok()?;
            self.scanned += chunk;
            bytes.extend_from_slice(&self.buf);
            self.buf = bytes;
        }
    }
}

fn extract_model_from_jsonl_tail(path: &Path) -> Result<Option<String>, StreamError> {
    Ok(extract_model_from_jsonl_reverse(path, None))
}

/// The model for a tool call in a JSONL transcript: the model on the
/// assistant message that issued `tool_use_id` when that message is in the
/// transcript, else the newest model the transcript names.
///
/// This reads whole lines backwards from the end of the file with no fixed
/// window, so large non-message records (tool results that carry a whole file,
/// file-history snapshots) between the assistant message and the end of the
/// file no longer hide the model.
pub fn extract_model_for_tool_use(
    path: &Path,
    format: StreamFormat,
    tool_use_id: Option<&str>,
) -> Result<Option<String>, StreamError> {
    match format {
        StreamFormat::ClaudeJsonl
        | StreamFormat::CopilotEventStreamJsonl
        | StreamFormat::GeminiJsonl
        | StreamFormat::CursorJsonl
        | StreamFormat::FirebenderJsonl => Ok(extract_model_from_jsonl_reverse(path, tool_use_id)),
        _ => extract_model(path, format, None),
    }
}

fn extract_model_from_jsonl_reverse(path: &Path, tool_use_id: Option<&str>) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut lines = ReverseLines::new(file)?;
    let tool_use_id = tool_use_id.filter(|id| !id.trim().is_empty());

    // The newest model in the transcript, and how many bytes had been read
    // when it was found.
    let mut newest: Option<(String, u64)> = None;

    while let Some(line) = lines.next_line() {
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };

        if let Some(id) = tool_use_id
            && text.contains(id)
            && let Some(model) = model_of_tool_use_line(text, id)
        {
            return Some(model);
        }

        if newest.is_none()
            && let Some(model) = extract_model_from_jsonl_line(text)
        {
            if tool_use_id.is_none() {
                return Some(model);
            }
            newest = Some((model, lines.scanned));
        }

        if let Some((_, found_at)) = &newest
            && lines.scanned.saturating_sub(*found_at) > TOOL_USE_SEARCH_BYTES
        {
            break;
        }
        if lines.scanned > MAX_REVERSE_SCAN_BYTES {
            break;
        }
    }

    if let Some((model, _)) = newest {
        return Some(model);
    }

    // Copilot CLI names the model (session.model_change) only at the start of
    // the session, which may lie beyond the scan limit on a huge transcript.
    if !lines.reached_start() {
        return extract_model_from_jsonl_head(path);
    }
    None
}

/// The model of an assistant record that issued tool call `tool_use_id`
/// (Claude: a `tool_use` content block with that `id`).
fn model_of_tool_use_line(line: &str, tool_use_id: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let message = json.get("message")?;
    let issued = message
        .get("content")
        .and_then(|c| c.as_array())
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                    && block.get("id").and_then(|i| i.as_str()) == Some(tool_use_id)
            })
        });
    if !issued {
        return None;
    }
    message
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| is_real_model(m))
        .map(String::from)
}

fn is_real_model(model: &str) -> bool {
    let m = model.trim();
    !m.is_empty() && m != "<synthetic>"
}

fn extract_model_from_jsonl_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.contains("odel") {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(trimmed).ok()?;

    if json.get("type").and_then(|v| v.as_str()) == Some("session.model_change")
        && let Some(model) = json
            .get("data")
            .and_then(|d| d.get("newModel"))
            .and_then(|v| v.as_str())
            .filter(|m| is_real_model(m))
    {
        return Some(model.to_string());
    }

    let data = json.get("data");
    let candidate = json
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(|v| v.as_str())
        .or_else(|| json.get("model").and_then(|v| v.as_str()))
        .or_else(|| json.get("model_id").and_then(|v| v.as_str()))
        // Copilot event streams keep per-event fields under `data`.
        .or_else(|| data.and_then(|d| d.get("model")).and_then(|v| v.as_str()))
        .or_else(|| data.and_then(|d| d.get("modelId")).and_then(|v| v.as_str()))
        .or_else(|| {
            data.and_then(|d| d.get("selectedModel"))
                .and_then(|v| v.as_str())
        });

    candidate.filter(|m| is_real_model(m)).map(String::from)
}

fn extract_model_from_jsonl_head(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file.take(MAX_HEAD_SCAN_BYTES));
    for line in reader.split(b'\n').map_while(Result::ok).take(200) {
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        if let Some(model) = extract_model_from_jsonl_line(text) {
            return Some(model);
        }
    }
    None
}

/// Extracts the model from VS Code Copilot's `models.json` debug log.
/// Given a transcript path like `.../transcripts/{session_id}.jsonl`,
/// derives `.../debug-logs/{session_id}/models.json` and reads the default model.
pub fn extract_model_from_copilot_models_json(
    stream_path: &Path,
) -> Result<Option<String>, StreamError> {
    let session_id = stream_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if session_id.is_empty() {
        return Ok(None);
    }

    // transcript: .../transcripts/{session_id}.jsonl
    // models:     .../debug-logs/{session_id}/models.json
    let transcripts_dir = match stream_path.parent() {
        Some(p) => p,
        None => return Ok(None),
    };
    let copilot_chat_dir = match transcripts_dir.parent() {
        Some(p) => p,
        None => return Ok(None),
    };
    let models_path = copilot_chat_dir
        .join("debug-logs")
        .join(session_id)
        .join("models.json");

    let content = match std::fs::read_to_string(&models_path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let models: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    let model = models.iter().find_map(|m| {
        if m.get("is_chat_default").and_then(|v| v.as_bool()) == Some(true) {
            m.get("id").and_then(|v| v.as_str()).map(String::from)
        } else {
            None
        }
    });

    Ok(model)
}

/// Most chat-session files the Copilot lookup opens, newest first.
const MAX_COPILOT_CHAT_SESSION_FILES: usize = 20;
/// Largest chat-session file the Copilot lookup reads.
const MAX_COPILOT_CHAT_SESSION_BYTES: u64 = 64 * 1024 * 1024;

/// The model of a VS Code Copilot agent session.
///
/// VS Code's agent transcript (`<workspaceStorage>/<ws>/GitHub.copilot-chat/
/// transcripts/<session>.jsonl`) names no model. The workspace's
/// `chatSessions/` files do: each request records its `modelId`, and a
/// finished request records the agent session in `result.metadata.sessionId`.
/// The chat whose requests name `session_id` is this session; its newest
/// request's model is the one in use, falling back to the chat's selected
/// model. The free-text `details` ("Claude Sonnet 4.5 • 0.9x") is not a model
/// id and is never used.
pub fn extract_model_from_copilot_chat_sessions(
    transcript_path: &Path,
    session_id: Option<&str>,
) -> Option<String> {
    let session_id = session_id
        .filter(|s| !s.trim().is_empty() && *s != "unknown")
        .map(str::to_string)
        .or_else(|| {
            transcript_path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })?;
    let chat_sessions_dir = transcript_path
        .parent()? // transcripts
        .parent()? // GitHub.copilot-chat
        .parent()? // <workspace storage id>
        .join("chatSessions");

    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> =
        std::fs::read_dir(&chat_sessions_dir)
            .ok()?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str())?;
                if ext != "json" && ext != "jsonl" {
                    return None;
                }
                let meta = entry.metadata().ok()?;
                if !meta.is_file() || meta.len() > MAX_COPILOT_CHAT_SESSION_BYTES {
                    return None;
                }
                Some((meta.modified().ok()?, path))
            })
            .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));

    for (_, path) in files.into_iter().take(MAX_COPILOT_CHAT_SESSION_FILES) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !content.contains(session_id.as_str()) {
            continue;
        }
        let is_jsonl = path.extension().and_then(|e| e.to_str()) == Some("jsonl");
        let Some(chat) = (if is_jsonl {
            replay_copilot_chat_session_jsonl(&content)
        } else {
            serde_json::from_str::<serde_json::Value>(&content).ok()
        }) else {
            continue;
        };
        if let Some(model) = copilot_chat_session_model(&chat, &session_id) {
            return Some(model);
        }
    }
    None
}

/// Rebuild a chat session from VS Code's incremental `chatSessions/*.jsonl`
/// log: kind 0 is the initial state, kind 1 sets the value at path `k`, kind 2
/// splices `v` into the array at `k` (from index `i` when given), kind 3
/// deletes the value at `k`.
fn replay_copilot_chat_session_jsonl(content: &str) -> Option<serde_json::Value> {
    let mut root: Option<serde_json::Value> = None;
    for line in content.lines() {
        let Ok(op) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let kind = op.get("kind").and_then(|k| k.as_u64());
        let value = op.get("v").cloned().unwrap_or(serde_json::Value::Null);
        if kind == Some(0) {
            root = Some(value);
            continue;
        }
        let (Some(root), Some(path)) = (root.as_mut(), op.get("k").and_then(|k| k.as_array()))
        else {
            continue;
        };
        let Some(target) = json_path_mut(root, path) else {
            continue;
        };
        match kind {
            Some(1) => *target = value,
            Some(2) => {
                if !target.is_array() {
                    *target = serde_json::Value::Array(Vec::new());
                }
                if let Some(arr) = target.as_array_mut() {
                    if let Some(i) = op.get("i").and_then(|i| i.as_u64()) {
                        arr.truncate(i as usize);
                    }
                    match value {
                        serde_json::Value::Array(items) => arr.extend(items),
                        other => arr.push(other),
                    }
                }
            }
            Some(3) => *target = serde_json::Value::Null,
            _ => {}
        }
    }
    root
}

/// The value at `path` (object keys and array indices), creating missing
/// object keys along the way.
fn json_path_mut<'a>(
    root: &'a mut serde_json::Value,
    path: &[serde_json::Value],
) -> Option<&'a mut serde_json::Value> {
    let mut current = root;
    for key in path {
        current = match key {
            serde_json::Value::String(k) => {
                if !current.is_object() {
                    *current = serde_json::Value::Object(serde_json::Map::new());
                }
                current
                    .as_object_mut()?
                    .entry(k.clone())
                    .or_insert(serde_json::Value::Null)
            }
            serde_json::Value::Number(n) => current.as_array_mut()?.get_mut(n.as_u64()? as usize)?,
            _ => return None,
        };
    }
    Some(current)
}

fn copilot_chat_session_model(chat: &serde_json::Value, session_id: &str) -> Option<String> {
    let requests = chat.get("requests").and_then(|r| r.as_array())?;
    let belongs = requests.iter().any(|req| {
        req.pointer("/result/metadata/sessionId")
            .and_then(|s| s.as_str())
            == Some(session_id)
    });
    if !belongs {
        return None;
    }
    let request_model = |req: &serde_json::Value| {
        req.get("modelId")
            .and_then(|m| m.as_str())
            .filter(|m| is_real_model(m))
            .map(String::from)
    };
    let selected = chat
        .pointer("/inputState/selectedModel/identifier")
        .and_then(|m| m.as_str())
        .filter(|m| is_real_model(m))
        .map(String::from);
    // The newest request is the one in flight; its model is the one in use.
    requests
        .last()
        .and_then(request_model)
        .or(selected)
        .or_else(|| requests.iter().rev().find_map(request_model))
}

fn extract_model_from_copilot_session_json(path: &Path) -> Result<Option<String>, StreamError> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    let model = json
        .get("requests")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|req| {
                req.get("modelId")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
        });

    Ok(model)
}

fn extract_model_from_amp_thread_json(path: &Path) -> Result<Option<String>, StreamError> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    let model = json
        .get("messages")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|msg| {
                msg.get("usage")
                    .and_then(|u| u.get("model"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
        });

    Ok(model)
}

fn extract_model_from_opencode_sqlite(
    path: &Path,
    session_id: Option<&str>,
) -> Result<Option<String>, StreamError> {
    let conn = match crate::streams::agents::opencode::open_sqlite_readonly(path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    // OpenCode stores model info in two places depending on message role:
    //   User messages:     data.model.modelID  (nested object)
    //   Assistant messages: data.modelID        (top-level string)
    let (query, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match session_id {
        Some(sid) => (
            "SELECT data FROM message WHERE session_id = ? AND (data LIKE '%\"modelID\"%' OR data LIKE '%\"model\"%') LIMIT 1",
            vec![Box::new(sid.to_string())],
        ),
        None => (
            "SELECT data FROM message WHERE (data LIKE '%\"modelID\"%' OR data LIKE '%\"model\"%') LIMIT 1",
            vec![],
        ),
    };

    let result: Option<String> = conn
        .query_row(query, rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|data| {
            let json: serde_json::Value = serde_json::from_str(&data).ok()?;
            // Try user message format: data.model.modelID
            if let Some(model) = json
                .get("model")
                .and_then(|m| m.get("modelID"))
                .and_then(|v| v.as_str())
            {
                return Some(model.to_string());
            }
            // Try assistant message format: data.modelID
            json.get("modelID")
                .and_then(|v| v.as_str())
                .map(String::from)
        });

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn test_extract_model_claude() {
        let path = fixture_path("example-claude-code.jsonl");
        let result = extract_model(&path, StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, Some("claude-sonnet-4-20250514".to_string()));
    }

    #[test]
    fn test_extract_model_droid_settings() {
        let path = fixture_path("droid-session.settings.json");
        let result = extract_model_from_droid_settings(&path).unwrap();
        assert_eq!(result, Some("custom:BYOK-GPT-5-MINI-0".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_session() {
        let path = fixture_path("copilot_session_simple.json");
        let result = extract_model(&path, StreamFormat::CopilotSessionJson, None).unwrap();
        assert_eq!(result, Some("copilot/claude-sonnet-4".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_event_stream() {
        let path = fixture_path("copilot_session_event_stream.jsonl");
        let result = extract_model(&path, StreamFormat::CopilotEventStreamJsonl, None).unwrap();
        // No model field in this fixture
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_gemini() {
        let path = fixture_path("gemini-session-simple.jsonl");
        let result = extract_model(&path, StreamFormat::GeminiJsonl, None).unwrap();
        assert_eq!(result, Some("gemini-2.5-flash".to_string()));
    }

    #[test]
    fn test_extract_model_amp() {
        let path = fixture_path("amp-threads/T-019ca1ce-3ae2-7686-a41e-ccc078837f8a.json");
        let result = extract_model(&path, StreamFormat::AmpThreadJson, None).unwrap();
        assert_eq!(result, Some("claude-opus-4-6".to_string()));
    }

    #[test]
    fn test_extract_model_opencode() {
        let path = fixture_path("opencode-sqlite/opencode.db");
        let result = extract_model(
            &path,
            StreamFormat::OpenCodeSqlite,
            Some("test-session-123"),
        )
        .unwrap();
        assert_eq!(result, Some("gpt-5".to_string()));
    }

    #[test]
    fn test_extract_model_opencode_assistant_message_format() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             INSERT INTO message VALUES ('msg-1', 'sess-1', 1000, 1000, '{\"role\":\"assistant\",\"modelID\":\"claude-opus-4-6\",\"providerID\":\"anthropic\"}');",
        ).unwrap();
        drop(conn);

        let result = extract_model(&db_path, StreamFormat::OpenCodeSqlite, Some("sess-1")).unwrap();
        assert_eq!(result, Some("claude-opus-4-6".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_cli() {
        let path = fixture_path("copilot_cli_session_events.jsonl");
        let result = extract_model(&path, StreamFormat::CopilotEventStreamJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-4.1".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_cli_no_model() {
        let path = fixture_path("copilot_cli_session_no_model.jsonl");
        let result = extract_model(&path, StreamFormat::CopilotEventStreamJsonl, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_cursor_jsonl() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"role":"assistant","message":{{"model":"composer-2","content":[{{"type":"text","text":"ok"}}]}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::CursorJsonl, None).unwrap();
        assert_eq!(result, Some("composer-2".to_string()));
    }

    #[test]
    fn test_extract_model_missing_file() {
        let path = PathBuf::from("/nonexistent/path/to/file.jsonl");
        let result = extract_model(&path, StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_empty_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let result = extract_model(file.path(), StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_droid_settings_missing_file() {
        let path = PathBuf::from("/nonexistent/settings.json");
        let result = extract_model_from_droid_settings(&path).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_unsupported_format_returns_none() {
        let path = fixture_path("example-claude-code.jsonl");
        let result = extract_model(&path, StreamFormat::DroidJsonl, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_claude_model_not_on_last_line() {
        let path = fixture_path("claude-model-not-last.jsonl");
        let result = extract_model(&path, StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, Some("claude-opus-4-6".to_string()));
    }

    #[test]
    fn test_extract_model_skips_synthetic_model() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"user","message":{{"content":"hello"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"assistant","message":{{"model":"claude-opus-4-6","content":[{{"type":"text","text":"hi"}}]}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"assistant","message":{{"model":"<synthetic>","content":[{{"type":"text","text":"bye"}}]}}}}"#).unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, Some("claude-opus-4-6".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_models_json() {
        let path = fixture_path(
            "copilot_vscode_workspace/GitHub.copilot-chat/transcripts/test-session-abc.jsonl",
        );
        let result = extract_model_from_copilot_models_json(&path).unwrap();
        assert_eq!(result, Some("gpt-4.1".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_models_json_missing() {
        let path = PathBuf::from("/nonexistent/transcripts/fake-session.jsonl");
        let result = extract_model_from_copilot_models_json(&path).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_head_fallback_for_large_file() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        // model_change at the start
        writeln!(file, r#"{{"type":"session.start","data":{{"sessionId":"s1"}},"id":"e1","timestamp":"2026-01-01T00:00:00Z","parentId":null}}"#).unwrap();
        writeln!(file, r#"{{"type":"session.model_change","data":{{"newModel":"gpt-4.1"}},"id":"e2","timestamp":"2026-01-01T00:00:01Z","parentId":"e1"}}"#).unwrap();
        // Pad with >50KB of filler events so the model_change falls outside the tail window
        for i in 0..600 {
            writeln!(file, r#"{{"type":"user.message","data":{{"content":"padding message number {} with extra text to make the line longer and push past the fifty kilobyte tail read window boundary"}},"id":"pad-{}","timestamp":"2026-01-01T00:01:{:02}Z","parentId":null}}"#, i, i, i % 60).unwrap();
        }
        file.flush().unwrap();

        let size = std::fs::metadata(file.path()).unwrap().len();
        assert!(
            size > 51200,
            "file must exceed 50KB tail window, got {}",
            size
        );

        let result =
            extract_model(file.path(), StreamFormat::CopilotEventStreamJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-4.1".to_string()));
    }

    fn write_lines(lines: &[String]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file.flush().unwrap();
        file
    }

    fn claude_tool_use(id: &str, model: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"model": model, "role": "assistant",
                "content": [{"type": "tool_use", "id": id, "name": "Write", "input": {}}]}
        })
        .to_string()
    }

    fn huge_tool_result(id: &str) -> String {
        serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id}]},
            "toolUseResult": {"content": "y".repeat(900 * 1024)}
        })
        .to_string()
    }

    #[test]
    fn test_reverse_scan_finds_model_behind_large_records() {
        // The assistant message sits behind several megabytes of tool results,
        // far outside the old 50 KB tail window, and the head holds none.
        let mut lines: Vec<String> = (0..30)
            .map(|i| format!(r#"{{"type":"progress","data":{{"i":{i}}}}}"#))
            .collect();
        lines.push(claude_tool_use("toolu_1", "claude-opus-4-7"));
        for i in 0..5 {
            lines.push(huge_tool_result(&format!("toolu_{i}")));
        }
        let file = write_lines(&lines);
        assert_eq!(
            extract_model(file.path(), StreamFormat::ClaudeJsonl, None).unwrap(),
            Some("claude-opus-4-7".to_string())
        );
    }

    #[test]
    fn test_tool_use_id_selects_its_own_assistant_message() {
        let lines = vec![
            claude_tool_use("toolu_a", "claude-opus-4-7"),
            huge_tool_result("toolu_a"),
            claude_tool_use("toolu_b", "claude-sonnet-4-5"),
            huge_tool_result("toolu_b"),
        ];
        let file = write_lines(&lines);
        let path = file.path();
        let for_tool = |id: Option<&str>| {
            extract_model_for_tool_use(path, StreamFormat::ClaudeJsonl, id)
                .unwrap()
                .unwrap()
        };
        assert_eq!(for_tool(Some("toolu_a")), "claude-opus-4-7");
        assert_eq!(for_tool(Some("toolu_b")), "claude-sonnet-4-5");
        // An id not in the transcript yet: the newest model.
        assert_eq!(for_tool(Some("toolu_missing")), "claude-sonnet-4-5");
        assert_eq!(for_tool(None), "claude-sonnet-4-5");
    }

    #[test]
    fn test_reverse_lines_handles_lines_across_chunks() {
        let long = "z".repeat(REVERSE_CHUNK_BYTES as usize * 2 + 17);
        let lines = vec![
            "first".to_string(),
            long.clone(),
            "".to_string(),
            "last".to_string(),
        ];
        let file = write_lines(&lines);
        let mut reader = ReverseLines::new(File::open(file.path()).unwrap()).unwrap();
        let mut seen = Vec::new();
        while let Some(line) = reader.next_line() {
            seen.push(String::from_utf8(line).unwrap());
        }
        assert_eq!(seen, vec!["last".to_string(), long, "first".to_string()]);
    }

    #[test]
    fn test_copilot_chat_sessions_ignore_chats_of_other_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let transcripts = ws.join("GitHub.copilot-chat").join("transcripts");
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::create_dir_all(ws.join("chatSessions")).unwrap();
        let transcript = transcripts.join("sess-1.jsonl");
        std::fs::write(&transcript, "").unwrap();
        // Mentions sess-1 only in text, and no request is linked to it.
        std::fs::write(
            ws.join("chatSessions").join("other.json"),
            r#"{"requests":[{"modelId":"copilot/gpt-4o","message":"see sess-1","result":{"metadata":{"sessionId":"sess-2"}}}]}"#,
        )
        .unwrap();
        assert_eq!(
            extract_model_from_copilot_chat_sessions(&transcript, Some("sess-1")),
            None
        );
    }
}
