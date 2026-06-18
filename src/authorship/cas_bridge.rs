//! Bridge AI prompt transcripts into the CAS (content-addressed storage) queue.
//!
//! Each AI checkpoint records the path to the agent's transcript file in its
//! `agent_metadata` (`"transcript_path"`). At post-commit time we read that
//! transcript, redact secrets, and enqueue it as a CAS object keyed by the hash
//! of its canonicalized content. The resulting `cas:<hash>` reference is written
//! onto the matching [`PromptRecord::messages_url`] so the authorship note can
//! point back at the full conversation that produced each AI attribution.
//!
//! The durable queue (`cas_sync_queue` in the internal DB) is drained and
//! uploaded by the daemon's telemetry flush loop.
//!
//! This is best-effort: any failure for a single session is swallowed so that
//! note generation is never blocked by transcript handling.

use std::collections::{BTreeMap, HashMap};

use crate::authorship::authorship_log::PromptRecord;
use crate::authorship::authorship_log_serialization::generate_short_hash;
use crate::authorship::internal_db::InternalDatabase;
use crate::authorship::working_log::{AgentId, Checkpoint};
use crate::error::AutterError;

/// Schema tag stored on each CAS transcript object so consumers (PR review,
/// blame, etc.) can evolve the shape over time.
const TRANSCRIPT_SCHEMA: &str = "cas/transcript/1.0.0";

/// For every AI `PromptRecord` that has a captured transcript, enqueue the
/// transcript as a CAS object and record the `cas:<hash>` on `messages_url`.
///
/// `checkpoints` is the working log for the commit being processed; it is the
/// only place the transcript file path is available (via `agent_metadata`).
pub fn enqueue_prompt_transcripts(
    prompts: &mut BTreeMap<String, PromptRecord>,
    checkpoints: &[Checkpoint],
) {
    if prompts.is_empty() {
        return;
    }

    // Map prompt short-hash -> transcript path from checkpoint agent metadata.
    // Checkpoints are in chronological order, so the latest path for a given
    // session wins (transcripts are append-only files keyed by session).
    let mut transcript_paths: HashMap<String, String> = HashMap::new();
    for cp in checkpoints {
        let (Some(agent_id), Some(meta)) = (&cp.agent_id, &cp.agent_metadata) else {
            continue;
        };
        if let Some(path) = meta.get("transcript_path") {
            let hash = generate_short_hash(&agent_id.id, &agent_id.tool);
            transcript_paths.insert(hash, path.clone());
        }
    }

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
        match enqueue_transcript_file(path, &record.agent_id) {
            Ok(Some(cas_hash)) => {
                record.messages_url = Some(format!("cas:{cas_hash}"));
            }
            // Transcript missing/empty, or enqueue failed: leave messages_url
            // unset. The note is still valid, just without a transcript link.
            Ok(None) | Err(_) => {}
        }
    }
}

/// Read and normalize a single transcript file, then enqueue it as a CAS object.
/// Returns the canonical content hash on success, or `None` when there is
/// nothing worth storing (file gone, empty, or unparseable).
fn enqueue_transcript_file(
    path: &str,
    agent_id: &AgentId,
) -> Result<Option<String>, AutterError> {
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

    // Redact secrets before the content ever leaves the local machine, matching
    // the redaction applied to streamed session events.
    let events: Vec<serde_json::Value> = events
        .into_iter()
        .map(crate::daemon::transcript_redaction::redact_json_secrets)
        .collect();

    let payload = serde_json::json!({
        "schema": TRANSCRIPT_SCHEMA,
        "tool": agent_id.tool,
        "model": agent_id.model,
        "session_id": agent_id.id,
        "events": events,
    });

    let mut metadata: HashMap<String, String> = HashMap::new();
    metadata.insert("kind".to_string(), "transcript".to_string());
    metadata.insert("tool".to_string(), agent_id.tool.clone());

    let db = InternalDatabase::global()?;
    let mut db_lock = db
        .lock()
        .map_err(|_| AutterError::Generic("CAS internal DB lock poisoned".to_string()))?;
    let cas_hash = db_lock.enqueue_cas_object(&payload, Some(&metadata))?;
    Ok(Some(cas_hash))
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
}
