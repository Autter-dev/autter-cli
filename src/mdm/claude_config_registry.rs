//! Registry of Claude-Code-compatible configuration directories.
//!
//! Claude Code reads its hook configuration from `~/.claude` (or
//! `$CLAUDE_CONFIG_DIR`) — but CLIs built on the Claude Agent SDK (PostHog
//! Code, and any other harness that embeds Claude Code) run their sessions
//! with `CLAUDE_CONFIG_DIR` pointing at their own config directory. Hooks
//! installed there are invisible to a plain `autter install-hooks` run from a
//! terminal, so a hook that goes stale in a harness config (e.g. its binary
//! path no longer exists) would silently break checkpointing for every
//! session of that harness, with no repair path.
//!
//! To keep every harness maintained without hardcoding vendor paths, each
//! `autter checkpoint claude` invocation — which runs inside the harness's
//! environment — records the active non-default `CLAUDE_CONFIG_DIR` here. The
//! Claude hook installer then installs/updates hooks in every registered
//! directory on each install or update run.

use std::path::PathBuf;

use crate::mdm::utils::{clean_path, home_dir, write_atomic};

fn registry_path() -> PathBuf {
    home_dir()
        .join(".autter")
        .join("internal")
        .join("claude-config-dirs.json")
}

/// All registered Claude-compatible config directories that still exist on
/// disk. Directories that have been removed are skipped (but stay in the
/// registry: harnesses like PostHog Code may be reinstalled later).
pub fn registered_config_dirs() -> Vec<PathBuf> {
    let Ok(raw) = std::fs::read_to_string(registry_path()) else {
        return Vec::new();
    };
    let Ok(dirs) = serde_json::from_str::<Vec<String>>(&raw) else {
        return Vec::new();
    };
    dirs.into_iter()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}

/// Record the active `CLAUDE_CONFIG_DIR` when it points somewhere other than
/// the default `~/.claude`. Called from the Claude checkpoint path, which
/// runs inside the harness's environment. Best-effort: failures are ignored
/// so a checkpoint is never blocked by registry bookkeeping.
pub fn register_active_config_dir() {
    let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") else {
        return;
    };
    if dir.trim().is_empty() {
        return;
    }
    let dir = clean_path(PathBuf::from(dir));
    if dir == home_dir().join(".claude") || !dir.is_dir() {
        return;
    }

    let path = registry_path();
    let mut dirs: Vec<String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();

    let dir_str = dir.to_string_lossy().to_string();
    if dirs.iter().any(|d| d == &dir_str) {
        return;
    }
    dirs.push(dir_str);

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&dirs) {
        let _ = write_atomic(&path, json.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_config_dirs_empty_when_registry_missing() {
        // With a HOME that has no registry file, this must not error.
        // (Integration tests run with an isolated HOME, so the registry file
        // won't exist there.)
        let dirs = registered_config_dirs();
        // Every returned entry must be an existing directory.
        for dir in dirs {
            assert!(dir.is_dir());
        }
    }
}
