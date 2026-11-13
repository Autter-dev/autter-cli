//! When an agent's checkpoint hook last ran, and what came of it.
//!
//! Hooks that are installed but never fire, or that fire and are dropped, look
//! the same from the outside: no attribution. `autter checkpoint` records each
//! run here for the agents listed in [`TRACKED_PRESETS`], so `autter doctor`
//! can say whether capture is working, not only whether it is installed.
//!
//! The file is `~/.autter/internal/hook_heartbeats/<preset>.json`. Writes are
//! best-effort and never fail the hook.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Presets whose hook runs are recorded. Kept to the agents doctor reports on.
pub const TRACKED_PRESETS: [&str; 1] = ["github-copilot"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOutcome {
    /// At least one file-edit or bash checkpoint was sent to the background service.
    Checkpoint,
    /// A prompt or turn-end event: the hook is wired up, but nothing was edited.
    Activity,
    /// The hook ran for a tool that doesn't edit files (a read or a search).
    Skipped,
    /// The hook ran for an edit, but the checkpoint was dropped.
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookHeartbeat {
    /// Unix seconds of the most recent hook run, whatever its outcome.
    pub last_run_at: u64,
    pub last_outcome: HookOutcome,
    /// Why the last run was skipped or dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_detail: Option<String>,
    /// Unix seconds of the most recent run that sent a checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checkpoint_at: Option<u64>,
    /// Unix seconds and reason of the most recent dropped edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

fn heartbeat_path(preset: &str) -> Option<PathBuf> {
    crate::config::internal_dir_path()
        .map(|dir| dir.join("hook_heartbeats").join(format!("{}.json", preset)))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether a preset error means "nothing to do" (a read or search tool) rather
/// than a dropped edit. Presets word those as "Skipping ...".
pub fn outcome_for_preset_error(message: &str) -> HookOutcome {
    if message.trim_start().starts_with("Skipping") {
        HookOutcome::Skipped
    } else {
        HookOutcome::Error
    }
}

/// Fold one hook run into the previous heartbeat.
pub fn next_heartbeat(
    previous: Option<HookHeartbeat>,
    outcome: HookOutcome,
    detail: Option<&str>,
    now: u64,
) -> HookHeartbeat {
    let previous = previous.unwrap_or(HookHeartbeat {
        last_run_at: now,
        last_outcome: outcome,
        last_detail: None,
        last_checkpoint_at: None,
        last_error_at: None,
        last_error: None,
    });
    let detail = detail.map(|d| d.chars().take(300).collect::<String>());
    HookHeartbeat {
        last_run_at: now,
        last_outcome: outcome,
        last_detail: detail.clone(),
        last_checkpoint_at: if outcome == HookOutcome::Checkpoint {
            Some(now)
        } else {
            previous.last_checkpoint_at
        },
        last_error_at: if outcome == HookOutcome::Error {
            Some(now)
        } else {
            previous.last_error_at
        },
        last_error: if outcome == HookOutcome::Error {
            detail
        } else {
            previous.last_error
        },
    }
}

/// Set by `autter doctor --agent` on the hook calls it simulates, so they
/// are never mistaken for the agent's own hook runs.
pub const SIMULATED_HOOK_ENV: &str = "AUTTER_SIMULATED_HOOK";

fn should_record(preset: &str, simulated: bool) -> bool {
    !simulated && TRACKED_PRESETS.contains(&preset)
}

/// Record one hook run for `preset`. A no-op for presets that aren't tracked,
/// and for the hook calls `autter doctor --agent` simulates.
pub fn record(preset: &str, outcome: HookOutcome, detail: Option<&str>) {
    if !should_record(preset, std::env::var_os(SIMULATED_HOOK_ENV).is_some()) {
        return;
    }
    let Some(path) = heartbeat_path(preset) else {
        return;
    };
    let next = next_heartbeat(read(preset), outcome, detail, now_secs());
    if let Ok(json) = serde_json::to_vec_pretty(&next) {
        let _ = crate::mdm::utils::write_atomic(&path, &json);
    }
}

/// The last recorded hook run for `preset`, if any.
pub fn read(preset: &str) -> Option<HookHeartbeat> {
    let path = heartbeat_path(preset)?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
pub fn describe_age(then: u64, now: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => {
            let days = secs / 86_400;
            if days == 1 {
                "1 day ago".to_string()
            } else {
                format!("{} days ago", days)
            }
        }
    }
}

/// Seconds since the Unix epoch, for callers comparing against a heartbeat.
pub fn now() -> u64 {
    now_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_messages_are_not_errors() {
        assert_eq!(
            outcome_for_preset_error(
                "Skipping VS Code hook for unsupported tool_name 'read_file' (non-edit tool)."
            ),
            HookOutcome::Skipped
        );
        assert_eq!(
            outcome_for_preset_error("cwd not found in hook_input"),
            HookOutcome::Error
        );
    }

    #[test]
    fn checkpoint_time_survives_later_skips_and_errors() {
        let first = next_heartbeat(None, HookOutcome::Checkpoint, None, 100);
        assert_eq!(first.last_checkpoint_at, Some(100));

        let skipped = next_heartbeat(Some(first), HookOutcome::Skipped, Some("read"), 200);
        assert_eq!(skipped.last_outcome, HookOutcome::Skipped);
        assert_eq!(skipped.last_checkpoint_at, Some(100));
        assert_eq!(skipped.last_error, None);

        let errored = next_heartbeat(Some(skipped), HookOutcome::Error, Some("no cwd"), 300);
        assert_eq!(errored.last_checkpoint_at, Some(100));
        assert_eq!(errored.last_error_at, Some(300));
        assert_eq!(errored.last_error.as_deref(), Some("no cwd"));

        let recovered = next_heartbeat(Some(errored), HookOutcome::Checkpoint, None, 400);
        assert_eq!(recovered.last_checkpoint_at, Some(400));
        assert_eq!(recovered.last_error_at, Some(300));
        assert_eq!(recovered.last_detail, None);
    }

    #[test]
    fn describe_age_buckets() {
        assert_eq!(describe_age(100, 130), "just now");
        assert_eq!(describe_age(0, 300), "5 min ago");
        assert_eq!(describe_age(0, 7200), "2 h ago");
        assert_eq!(describe_age(0, 86_400), "1 day ago");
        assert_eq!(describe_age(0, 3 * 86_400), "3 days ago");
    }

    #[test]
    fn untracked_presets_are_ignored() {
        assert!(!TRACKED_PRESETS.contains(&"claude"));
        assert!(TRACKED_PRESETS.contains(&"github-copilot"));
        // A hook call simulated by `autter doctor --agent` must not read as
        // Copilot capture working.
        assert!(should_record("github-copilot", false));
        assert!(!should_record("github-copilot", true));
        assert!(!should_record("claude", false));
    }
}
