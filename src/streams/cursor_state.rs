//! Cursor conversations from Cursor's own database, for when Cursor's
//! transcripts setting is off.
//!
//! Cursor writes `~/.cursor/projects/**/<conversationId>.jsonl` transcripts only
//! when its transcripts setting is on, and then its hooks pass
//! `transcript_path`. Without the setting the hooks pass null, but Cursor still
//! keeps every conversation in its global state database
//! (`<config dir>/Cursor/User/globalStorage/state.vscdb`, table `cursorDiskKV`):
//!
//! - `composerData:<conversationId>`: the conversation, with
//!   `fullConversationHeadersOnly: [{bubbleId, type}]` in order (older builds
//!   inline the messages as `conversation: [{bubbleId, type, text}]`).
//! - `bubbleId:<conversationId>:<bubbleId>`: one message, `type` 1 (user) or 2
//!   (assistant) and its `text`.
//!
//! On a turn-end (`stop`) hook with no transcript, autter appends the
//! conversation's new messages to its own Cursor-shaped JSONL file
//! (`~/.autter/cursor-transcripts/<conversationId>.jsonl`, one
//! `{"role", "message": {"content": [{"type": "text", "text"}]}}` per message)
//! and hands that file to the regular Cursor transcript reader. The file is
//! append-only, so the reader's byte-offset watermark holds.
//!
//! The database is Cursor's private storage and undocumented; every failure
//! here (no database, an unknown layout) only means no prompts for the turn.

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Where autter keeps the transcripts it copies out of Cursor's database.
pub fn exported_transcripts_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("AUTTER_CURSOR_EXPORT_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|home| home.join(".autter").join("cursor-transcripts"))
}

/// Cursor's global state database for this user.
pub fn state_db_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CURSOR_STATE_DB") {
        return Some(PathBuf::from(path));
    }
    dirs::config_dir().map(|dir| {
        dir.join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorMessage {
    pub bubble_id: String,
    /// "user" or "assistant".
    pub role: &'static str,
    pub text: String,
    pub created_at: Option<serde_json::Value>,
}

fn value_text(value: rusqlite::types::Value) -> Option<String> {
    match value {
        rusqlite::types::Value::Text(text) => Some(text),
        rusqlite::types::Value::Blob(bytes) => String::from_utf8(bytes).ok(),
        _ => None,
    }
}

fn role_of(kind: &serde_json::Value) -> Option<&'static str> {
    match kind.as_i64() {
        Some(1) => Some("user"),
        Some(2) => Some("assistant"),
        _ => None,
    }
}

fn message_from_bubble(bubble: &serde_json::Value, fallback_id: &str) -> Option<CursorMessage> {
    let role = role_of(bubble.get("type")?)?;
    let text = bubble.get("text")?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    let bubble_id = bubble
        .get("bubbleId")
        .and_then(|v| v.as_str())
        .unwrap_or(fallback_id)
        .to_string();
    Some(CursorMessage {
        bubble_id,
        role,
        text: text.to_string(),
        created_at: bubble.get("createdAt").cloned(),
    })
}

/// The conversation's messages with text, in order; None when the database
/// has no such conversation.
pub fn read_conversation(
    conn: &Connection,
    conversation_id: &str,
) -> rusqlite::Result<Option<Vec<CursorMessage>>> {
    let composer: Option<rusqlite::types::Value> = conn
        .query_row(
            "SELECT value FROM cursorDiskKV WHERE key = ?1",
            [format!("composerData:{conversation_id}")],
            |row| row.get(0),
        )
        .optional()?;
    let Some(composer) = composer.and_then(value_text) else {
        return Ok(None);
    };
    let Ok(composer) = serde_json::from_str::<serde_json::Value>(&composer) else {
        return Ok(None);
    };

    // Older builds keep the messages inline.
    if let Some(inline) = composer.get("conversation").and_then(|v| v.as_array())
        && !inline.is_empty()
    {
        return Ok(Some(
            inline
                .iter()
                .enumerate()
                .filter_map(|(i, b)| message_from_bubble(b, &format!("inline-{i}")))
                .collect(),
        ));
    }

    let Some(headers) = composer
        .get("fullConversationHeadersOnly")
        .and_then(|v| v.as_array())
    else {
        return Ok(Some(Vec::new()));
    };

    // One indexed range scan for every message of the conversation.
    let prefix = format!("bubbleId:{conversation_id}:");
    let mut stmt =
        conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")?;
    let upper = format!("bubbleId:{conversation_id};");
    let mut bubbles = std::collections::HashMap::new();
    let rows = stmt.query_map([prefix.as_str(), upper.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, rusqlite::types::Value>(1)?,
        ))
    })?;
    for row in rows {
        let (key, value) = row?;
        let Some(bubble_id) = key.strip_prefix(&prefix) else {
            continue;
        };
        if let Some(parsed) = value_text(value).and_then(|t| serde_json::from_str(&t).ok()) {
            bubbles.insert(bubble_id.to_string(), parsed);
        }
    }

    let messages = headers
        .iter()
        .filter_map(|header| {
            let bubble_id = header.get("bubbleId")?.as_str()?;
            let bubble: &serde_json::Value = bubbles.get(bubble_id)?;
            message_from_bubble(bubble, bubble_id)
        })
        .collect();
    Ok(Some(messages))
}

fn exported_bubble_ids(path: &Path) -> HashSet<String> {
    let Ok(file) = fs::File::open(path) else {
        return HashSet::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .filter_map(|v| v.get("bubbleId")?.as_str().map(str::to_string))
        .collect()
}

/// One Cursor-transcript-shaped line for a message.
pub fn transcript_line(message: &CursorMessage) -> serde_json::Value {
    let mut line = serde_json::json!({
        "role": message.role,
        "message": { "content": [{ "type": "text", "text": message.text }] },
        "bubbleId": message.bubble_id,
        "source": "cursor-state-db",
    });
    if let Some(created_at) = &message.created_at {
        line["createdAt"] = created_at.clone();
    }
    line
}

/// Append the conversation's new messages to its exported transcript and
/// return the file, or None when Cursor's database has no such conversation.
pub fn export_conversation(
    db_path: &Path,
    conversation_id: &str,
    out_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    if conversation_id.is_empty()
        || !conversation_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "unexpected Cursor conversation id: {conversation_id}"
        ));
    }
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open {}: {e}", db_path.display()))?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(2_000));
    let Some(messages) = read_conversation(&conn, conversation_id)
        .map_err(|e| format!("read Cursor conversation {conversation_id}: {e}"))?
    else {
        return Ok(None);
    };

    fs::create_dir_all(out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;
    let path = out_dir.join(format!("{conversation_id}.jsonl"));
    let done = exported_bubble_ids(&path);
    let mut fresh = String::new();
    for message in messages.iter().filter(|m| !done.contains(&m.bubble_id)) {
        fresh.push_str(&transcript_line(message).to_string());
        fresh.push('\n');
    }
    if !fresh.is_empty() || !path.exists() {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        file.write_all(fresh.as_bytes())
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(Some(path))
}

/// [`export_conversation`] with the default locations; None on any failure.
pub fn export_for_hook(conversation_id: &str) -> Option<PathBuf> {
    let db = state_db_path()?;
    let out = exported_transcripts_dir()?;
    match export_conversation(&db, conversation_id, &out) {
        Ok(path) => path,
        Err(e) => {
            tracing::debug!(error = %e, "Cursor state database export failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_state_db(dir: &Path) -> PathBuf {
        let path = dir.join("state.vscdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
        )
        .unwrap();
        let put = |key: &str, value: serde_json::Value| {
            conn.execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, value.to_string().into_bytes()],
            )
            .unwrap();
        };
        put(
            "composerData:conv-1",
            serde_json::json!({
                "composerId": "conv-1",
                "fullConversationHeadersOnly": [
                    {"bubbleId": "b1", "type": 1},
                    {"bubbleId": "b2", "type": 2},
                    {"bubbleId": "b3", "type": 2},
                ]
            }),
        );
        put(
            "bubbleId:conv-1:b1",
            serde_json::json!({"type": 1, "text": "Add retries to the webhook sender", "createdAt": "2026-10-25T10:00:00Z"}),
        );
        put(
            "bubbleId:conv-1:b2",
            serde_json::json!({"type": 2, "text": "Done: added backoff."}),
        );
        // A tool-only assistant bubble has no text.
        put(
            "bubbleId:conv-1:b3",
            serde_json::json!({"type": 2, "text": ""}),
        );
        // Another conversation's bubble must not leak in.
        put(
            "bubbleId:conv-10:x",
            serde_json::json!({"type": 1, "text": "other"}),
        );
        path
    }

    #[test]
    fn exports_messages_in_order_and_appends_only_new_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let db = fake_state_db(tmp.path());
        let out = tmp.path().join("out");

        let path = export_conversation(&db, "conv-1", &out).unwrap().unwrap();
        let first = fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = first
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["role"], "user");
        assert_eq!(
            lines[0]["message"]["content"][0]["text"],
            "Add retries to the webhook sender"
        );
        assert_eq!(lines[0]["createdAt"], "2026-10-25T10:00:00Z");
        assert_eq!(lines[1]["role"], "assistant");

        // The next turn adds a message; only it is appended.
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:conv-1', ?1)",
            [serde_json::json!({"fullConversationHeadersOnly": [
                {"bubbleId": "b1", "type": 1}, {"bubbleId": "b2", "type": 2}, {"bubbleId": "b4", "type": 1}
            ]})
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES ('bubbleId:conv-1:b4', ?1)",
            [serde_json::json!({"type": 1, "text": "No, use lib/retry.ts"}).to_string()],
        )
        .unwrap();
        drop(conn);
        export_conversation(&db, "conv-1", &out).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert!(second.starts_with(&first), "the file only grows");
        assert_eq!(second.lines().count(), 3);
        assert!(
            second
                .lines()
                .last()
                .unwrap()
                .contains("No, use lib/retry.ts")
        );
    }

    #[test]
    fn reads_inline_conversations_and_reports_missing_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let db = fake_state_db(tmp.path());
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:old', ?1)",
            [serde_json::json!({"conversation": [
                {"bubbleId": "o1", "type": 1, "text": "old style ask"},
                {"bubbleId": "o2", "type": 2, "text": "old style answer"}
            ]})
            .to_string()],
        )
        .unwrap();
        let messages = read_conversation(&conn, "old").unwrap().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "old style ask");
        assert!(read_conversation(&conn, "nope").unwrap().is_none());
        drop(conn);
        let out = tmp.path().join("out");
        assert!(export_conversation(&db, "nope", &out).unwrap().is_none());
        assert!(
            export_conversation(&tmp.path().join("missing.vscdb"), "conv-1", &out)
                .unwrap()
                .is_none()
        );
        assert!(export_conversation(&db, "../escape", &out).is_err());
    }
}
