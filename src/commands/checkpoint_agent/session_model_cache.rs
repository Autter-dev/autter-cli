//! The last model each agent session was seen using, kept on disk.
//!
//! A preset reads the model from the hook payload or the agent's transcript.
//! Sometimes neither has it yet (the transcript is not flushed, or the agent
//! never writes the model where we look), and the checkpoint would be recorded
//! as `<tool>/unknown-model`. When an earlier checkpoint of the same session
//! knew the model, that model is the best answer, so every checkpoint that
//! finds a model records it here and every checkpoint that does not reads it
//! back.
//!
//! One small file per session under `~/.autter/internal/session-models/`,
//! written atomically, so parallel hook processes and service restarts are
//! both safe.

use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::working_log::{AgentId, is_unknown_model};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Entries untouched for this long are removed when a new session is recorded.
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    tool: String,
    session_id: String,
    model: String,
}

fn cache_dir() -> Option<PathBuf> {
    // Integration tests pin the directory: the home directory ignores HOME on
    // Windows, so a test HOME alone would not isolate it.
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("AUTTER_TEST_SESSION_MODELS_PATH") {
        return Some(PathBuf::from(path));
    }
    crate::config::internal_dir_path().map(|dir| dir.join("session-models"))
}

fn entry_path(tool: &str, session_id: &str) -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join(format!("{}.json", generate_session_id(session_id, tool))))
}

fn is_cacheable_session(agent_id: &AgentId) -> bool {
    let id = agent_id.id.trim();
    !id.is_empty() && id != "unknown" && !agent_id.tool.trim().is_empty()
}

/// The last model recorded for this agent session, if any.
pub fn recall(agent_id: &AgentId) -> Option<String> {
    if !is_cacheable_session(agent_id) {
        return None;
    }
    let content = fs::read_to_string(entry_path(&agent_id.tool, &agent_id.id)?).ok()?;
    let entry: Entry = serde_json::from_str(&content).ok()?;
    (entry.tool == agent_id.tool
        && entry.session_id == agent_id.id
        && !is_unknown_model(&entry.model))
    .then_some(entry.model)
}

/// Record the model this agent session is using. Unknown models are ignored.
pub fn remember(agent_id: &AgentId) {
    if !is_cacheable_session(agent_id) || !agent_id.has_known_model() {
        return;
    }
    let Some(path) = entry_path(&agent_id.tool, &agent_id.id) else {
        return;
    };
    if recall(agent_id).as_deref() == Some(agent_id.model.as_str()) {
        return;
    }
    let Some(dir) = path.parent() else {
        return;
    };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let is_new = !path.exists();
    let entry = Entry {
        tool: agent_id.tool.clone(),
        session_id: agent_id.id.clone(),
        model: agent_id.model.clone(),
    };
    let Ok(json) = serde_json::to_string(&entry) else {
        return;
    };
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("entry"),
        std::process::id()
    ));
    if fs::write(&tmp, json).is_err() || fs::rename(&tmp, &path).is_err() {
        let _ = fs::remove_file(&tmp);
        return;
    }
    if is_new {
        prune_stale(dir);
    }
}

/// Fill an unknown model from the session's last known model, or record a
/// known one. Returns true when the model was filled in.
pub fn reconcile(agent_id: &mut AgentId) -> bool {
    if agent_id.has_known_model() {
        remember(agent_id);
        return false;
    }
    match recall(agent_id) {
        Some(model) => {
            agent_id.model = model;
            true
        }
        None => false,
    }
}

fn prune_stale(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > MAX_AGE);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}
