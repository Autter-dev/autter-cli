//! Google Antigravity hook installer.
//!
//! Sources (verified 2026-10-08):
//! - Hooks: https://antigravity.google/docs/hooks. The global hooks file is
//!   `~/.gemini/config/hooks.json`, shared by Antigravity 2.0, the Antigravity
//!   CLI and the Antigravity IDE (workspace files live at `.agents/hooks.json`).
//!   The file maps hook *names* to event configs:
//!   `{"<name>": {"PreToolUse": [{"matcher": "<regex>", "hooks": [{"type":
//!   "command", "command": "...", "timeout": 30}]}], "PostToolUse": [...]}}`.
//!   File-editing tools: `write_to_file`, `replace_file_content`,
//!   `multi_replace_file_content`; shell: `run_command`.
//! - App data dirs (same page, `transcriptPath` note; and
//!   https://antigravity.google/docs/settings): `~/.gemini/antigravity`
//!   (Antigravity 2.0), `~/.gemini/antigravity-cli` (CLI),
//!   `~/.gemini/antigravity-ide` (IDE).
//! - CLI launcher: `agy`, installed to `~/.local/bin/agy` on macOS/Linux and
//!   `%LOCALAPPDATA%\agy\bin` on Windows
//!   (https://antigravity.google/docs/cli/install).
//!
//! The desktop app bundle and install folder names are not documented, so they
//! are not used as detection signals. `~/.gemini` on its own belongs to the
//! Gemini CLI and is not a signal either.
//!
//! Known gap: public reports (Jul–Aug 2026) found that the Antigravity IDE did
//! not execute hooks even though the docs list it; the CLI does, and the 2.0
//! changelog shows hooks running. See docs/unsupported-agents.md.

use crate::error::AutterError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{binary_exists, generate_diff, home_dir, write_atomic};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// Top-level hook name autter owns in `hooks.json`.
const HOOK_NAME: &str = "autter";

/// Tools whose calls are checkpointed (regex alternation, matched exactly).
const TOOL_MATCHER: &str =
    "write_to_file|replace_file_content|multi_replace_file_content|run_command";

const HOOK_TIMEOUT_SECS: u64 = 30;

pub struct AntigravityInstaller;

impl AntigravityInstaller {
    fn hooks_path(home: &Path) -> PathBuf {
        home.join(".gemini").join("config").join("hooks.json")
    }

    /// Verified signals that some Antigravity surface is installed.
    pub(crate) fn detected_in(home: &Path, on_path: &dyn Fn(&str) -> bool) -> bool {
        let app_data = home.join(".gemini");
        let agy_unix = home.join(".local").join("bin").join("agy");
        let agy_windows = std::env::var_os("LOCALAPPDATA")
            .map(|dir| PathBuf::from(dir).join("agy").join("bin").join("agy.exe"));
        on_path("agy")
            || agy_unix.is_file()
            || agy_windows.is_some_and(|p| p.is_file())
            || ["antigravity", "antigravity-cli", "antigravity-ide"]
                .iter()
                .any(|dir| app_data.join(dir).is_dir())
    }

    /// The `autter` hook entry for this binary.
    fn desired_entry(binary: &Path) -> Value {
        let handler = |preset: &str| {
            json!({
                "type": "command",
                "command": format!("{} checkpoint {} --hook-input stdin", binary.display(), preset),
                "timeout": HOOK_TIMEOUT_SECS,
            })
        };
        json!({
            "PreToolUse": [{"matcher": TOOL_MATCHER, "hooks": [handler("antigravity-pre")]}],
            "PostToolUse": [{"matcher": TOOL_MATCHER, "hooks": [handler("antigravity-post")]}],
        })
    }

    fn read(path: &Path) -> Result<(String, Value), AutterError> {
        if !path.exists() {
            return Ok((String::new(), json!({})));
        }
        let content = fs::read_to_string(path)?;
        let value = if content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&content)?
        };
        Ok((content, value))
    }

    pub(crate) fn install_at(
        path: &Path,
        binary: &Path,
        dry_run: bool,
    ) -> Result<Option<String>, AutterError> {
        let (existing_content, existing) = Self::read(path)?;
        let mut merged = existing.clone();
        let Some(root) = merged.as_object_mut() else {
            return Err(AutterError::Generic(format!(
                "{} is not a JSON object",
                path.display()
            )));
        };
        root.insert(HOOK_NAME.to_string(), Self::desired_entry(binary));
        if merged == existing {
            return Ok(None);
        }
        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff = generate_diff(path, &existing_content, &new_content);
        if !dry_run {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            write_atomic(path, new_content.as_bytes())?;
        }
        Ok(Some(diff))
    }

    pub(crate) fn uninstall_at(path: &Path, dry_run: bool) -> Result<Option<String>, AutterError> {
        if !path.exists() {
            return Ok(None);
        }
        let (existing_content, existing) = Self::read(path)?;
        let mut merged = existing.clone();
        let removed = merged
            .as_object_mut()
            .and_then(|root| root.remove(HOOK_NAME))
            .is_some();
        if !removed {
            return Ok(None);
        }
        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff = generate_diff(path, &existing_content, &new_content);
        if !dry_run {
            write_atomic(path, new_content.as_bytes())?;
        }
        Ok(Some(diff))
    }

    /// `(installed, up_to_date)` for the hooks file at `path`.
    pub(crate) fn status_at(path: &Path, binary: &Path) -> (bool, bool) {
        let Ok((_, value)) = Self::read(path) else {
            return (false, false);
        };
        match value.get(HOOK_NAME) {
            Some(entry) => (true, *entry == Self::desired_entry(binary)),
            None => (false, false),
        }
    }
}

impl HookInstaller for AntigravityInstaller {
    fn name(&self) -> &str {
        "Google Antigravity"
    }

    fn id(&self) -> &str {
        "antigravity"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["Antigravity", "antigravity", "agy"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, AutterError> {
        let home = home_dir();
        if !Self::detected_in(&home, &binary_exists) {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }
        let (installed, up_to_date) =
            Self::status_at(&Self::hooks_path(&home), &params.binary_path);
        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: installed,
            hooks_up_to_date: up_to_date,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, AutterError> {
        Self::install_at(&Self::hooks_path(&home_dir()), &params.binary_path, dry_run)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, AutterError> {
        Self::uninstall_at(&Self::hooks_path(&home_dir()), dry_run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn gemini_cli_only_home_is_not_antigravity() {
        // A Gemini-CLI-only ~/.gemini (extensions/ and settings.json).
        let home = tempfile::tempdir().unwrap();
        let gemini = home.path().join(".gemini");
        fs::create_dir_all(gemini.join("extensions")).unwrap();
        fs::write(gemini.join("settings.json"), "{}").unwrap();
        assert!(!AntigravityInstaller::detected_in(home.path(), &none));
    }

    #[test]
    fn each_verified_signal_detects_antigravity() {
        for dir in ["antigravity", "antigravity-cli", "antigravity-ide"] {
            let home = tempfile::tempdir().unwrap();
            fs::create_dir_all(home.path().join(".gemini").join(dir)).unwrap();
            assert!(
                AntigravityInstaller::detected_in(home.path(), &none),
                "{dir}"
            );
        }
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".local/bin")).unwrap();
        fs::write(home.path().join(".local/bin/agy"), "").unwrap();
        assert!(AntigravityInstaller::detected_in(home.path(), &none));

        let home = tempfile::tempdir().unwrap();
        assert!(AntigravityInstaller::detected_in(home.path(), &|n| n == "agy"));
    }

    #[test]
    fn install_preserves_other_hooks_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config").join("hooks.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"my-linter-hook": {"PostToolUse": [{"matcher": "run_command", "hooks": [{"command": "./lint.sh"}]}]}}"#,
        )
        .unwrap();
        let binary = Path::new("/usr/local/bin/autter");

        assert!(
            AntigravityInstaller::install_at(&path, binary, false)
                .unwrap()
                .is_some()
        );
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(written.get("my-linter-hook").is_some(), "user hook kept");
        let pre = &written["autter"]["PreToolUse"][0];
        assert_eq!(pre["matcher"], TOOL_MATCHER);
        assert_eq!(
            pre["hooks"][0]["command"],
            "/usr/local/bin/autter checkpoint antigravity-pre --hook-input stdin"
        );
        assert_eq!(
            written["autter"]["PostToolUse"][0]["hooks"][0]["command"],
            "/usr/local/bin/autter checkpoint antigravity-post --hook-input stdin"
        );
        assert_eq!(AntigravityInstaller::status_at(&path, binary), (true, true));

        // Second install changes nothing.
        assert!(
            AntigravityInstaller::install_at(&path, binary, false)
                .unwrap()
                .is_none()
        );
        // A different binary path is reported as out of date.
        assert_eq!(
            AntigravityInstaller::status_at(&path, Path::new("/opt/autter")),
            (true, false)
        );

        assert!(
            AntigravityInstaller::uninstall_at(&path, false)
                .unwrap()
                .is_some()
        );
        let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(after.get("autter").is_none());
        assert!(after.get("my-linter-hook").is_some());
    }

    #[test]
    fn install_creates_missing_config_dir_and_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".gemini").join("config").join("hooks.json");
        let binary = Path::new("/usr/local/bin/autter");
        assert!(
            AntigravityInstaller::install_at(&path, binary, true)
                .unwrap()
                .is_some()
        );
        assert!(!path.exists(), "dry run must not write");
        AntigravityInstaller::install_at(&path, binary, false).unwrap();
        assert!(path.exists());
    }
}
