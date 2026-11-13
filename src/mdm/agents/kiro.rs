//! Kiro hook installer (Kiro CLI v3).
//!
//! Sources (fetched 2026-11-12):
//! - https://kiro.dev/docs/hooks/ : standalone hook files are JSON documents
//!   `{"version": "v1", "hooks": [{"name", "trigger", "matcher", "action":
//!   {"type": "command", "command"}, "timeout", "enabled"}]}`; "Kiro CLI V3
//!   also loads global Hooks from `~/.kiro/hooks/`". The docs do not say the
//!   Kiro IDE loads that folder (unconfirmed; see docs/kiro-capture.md).
//! - https://kiro.dev/docs/cli/v3/hooks-migration/ : CLI 2.x kept hooks inside
//!   each agent profile and has no global hooks folder, so it is not
//!   supported; `write` covers `fs_write` and `code`, `shell` covers
//!   `execute_bash`.
//!
//! autter owns one file, `~/.kiro/hooks/autter.json`, and never edits the
//! user's own hook files or writes `.kiro/hooks/` into repositories (those
//! files are committed, and a teammate without autter would have every edit
//! fail its hook).

use crate::error::AutterError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{
    binary_exists, generate_diff, home_dir, normalize_windows_path_for_shell, write_atomic,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// Preset the hook commands run.
const PRESET: &str = "kiro";

/// Seconds Kiro waits for each hook (its default is 60).
const HOOK_TIMEOUT_SECS: u64 = 30;

pub struct KiroInstaller;

impl KiroInstaller {
    fn hooks_dir(home: &Path) -> PathBuf {
        home.join(".kiro").join("hooks")
    }

    fn hooks_path(home: &Path) -> PathBuf {
        Self::hooks_dir(home).join("autter.json")
    }

    /// Signals that Kiro is installed: the CLI (`kiro-cli`, or `kiro` on
    /// PATH), the IDE app or its VS Code-style config folder, or `~/.kiro`.
    pub(crate) fn detected_in(home: &Path, on_path: &dyn Fn(&str) -> bool) -> bool {
        let mut app_dirs = vec![PathBuf::from("/Applications"), home.join("Applications")];
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            app_dirs.push(PathBuf::from(local).join("Programs"));
        }
        on_path("kiro-cli")
            || on_path("kiro")
            || home.join(".kiro").is_dir()
            || home.join(".local").join("bin").join("kiro-cli").is_file()
            || app_dirs
                .iter()
                .any(|dir| dir.join("Kiro.app").is_dir() || dir.join("Kiro").is_dir())
            || Self::config_dirs(home)
                .iter()
                .any(|dir| dir.join("Kiro").join("User").is_dir())
    }

    /// Where VS Code-style editors keep `<product>/User`.
    fn config_dirs(home: &Path) -> Vec<PathBuf> {
        let mut dirs = vec![
            home.join("Library").join("Application Support"),
            home.join(".config"),
        ];
        if let Some(appdata) = std::env::var_os("APPDATA") {
            dirs.push(PathBuf::from(appdata));
        }
        dirs
    }

    /// The hook file for this binary. Windows paths use forward slashes, as in
    /// every other agent's hooks.
    fn desired_file(binary: &Path) -> Value {
        let binary = normalize_windows_path_for_shell(binary);
        let command = format!("{binary} checkpoint {PRESET} --hook-input stdin");
        let hook = |name: &str, description: &str, trigger: &str, matcher: &str| {
            json!({
                "name": name,
                "description": description,
                "trigger": trigger,
                "matcher": matcher,
                "action": {"type": "command", "command": command},
                "timeout": HOOK_TIMEOUT_SECS,
                "enabled": true,
            })
        };
        json!({
            "version": "v1",
            "hooks": [
                hook("autter: before file write",
                     "Records the file before the agent edits it (autter install-hooks)",
                     "PreToolUse", "write"),
                hook("autter: after file write",
                     "Records the agent's edit (autter install-hooks)",
                     "PostToolUse", "write"),
                hook("autter: before shell command",
                     "Records files before an agent shell command (autter install-hooks)",
                     "PreToolUse", "shell"),
                hook("autter: after shell command",
                     "Records files an agent shell command changed (autter install-hooks)",
                     "PostToolUse", "shell"),
            ],
        })
    }

    fn read(path: &Path) -> Result<(String, Option<Value>), AutterError> {
        if !path.exists() {
            return Ok((String::new(), None));
        }
        let content = fs::read_to_string(path)?;
        let value = serde_json::from_str(&content).ok();
        Ok((content, value))
    }

    pub(crate) fn install_at(
        path: &Path,
        binary: &Path,
        dry_run: bool,
    ) -> Result<Option<String>, AutterError> {
        let (existing_content, existing) = Self::read(path)?;
        let desired = Self::desired_file(binary);
        if existing.as_ref() == Some(&desired) {
            return Ok(None);
        }
        let new_content = serde_json::to_string_pretty(&desired)?;
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
        let (existing_content, _) = Self::read(path)?;
        let diff = generate_diff(path, &existing_content, "");
        if !dry_run {
            fs::remove_file(path)?;
        }
        Ok(Some(diff))
    }

    /// `(installed, up_to_date)`: installed when autter's file runs an autter
    /// checkpoint for Kiro; up to date only when it is exactly the file this
    /// binary would write, so a moved binary counts as out of date.
    pub(crate) fn status_at(path: &Path, binary: &Path) -> (bool, bool) {
        let Ok((content, value)) = Self::read(path) else {
            return (false, false);
        };
        let installed = content.contains(&format!(" checkpoint {PRESET} "));
        let up_to_date = value.as_ref() == Some(&Self::desired_file(binary));
        (installed, installed && up_to_date)
    }
}

impl HookInstaller for KiroInstaller {
    fn name(&self) -> &str {
        "Kiro"
    }

    fn id(&self) -> &str {
        "kiro"
    }

    fn hook_config_paths(&self) -> Vec<PathBuf> {
        vec![Self::hooks_path(&home_dir())]
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["Kiro", "kiro", "kiro-cli"]
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
    fn each_signal_detects_kiro() {
        let home = tempfile::tempdir().unwrap();
        assert!(!KiroInstaller::detected_in(home.path(), &none));
        fs::create_dir_all(home.path().join(".kiro")).unwrap();
        assert!(KiroInstaller::detected_in(home.path(), &none));

        let home = tempfile::tempdir().unwrap();
        assert!(KiroInstaller::detected_in(home.path(), &|n| n == "kiro-cli"));
        assert!(KiroInstaller::detected_in(home.path(), &|n| n == "kiro"));

        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join("Applications/Kiro.app")).unwrap();
        assert!(KiroInstaller::detected_in(home.path(), &none));

        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".config/Kiro/User")).unwrap();
        assert!(KiroInstaller::detected_in(home.path(), &none));

        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".local/bin")).unwrap();
        fs::write(home.path().join(".local/bin/kiro-cli"), "").unwrap();
        assert!(KiroInstaller::detected_in(home.path(), &none));
    }

    #[test]
    fn install_writes_one_file_and_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let path = KiroInstaller::hooks_path(home.path());
        let binary = Path::new("/usr/local/bin/autter");

        assert!(
            KiroInstaller::install_at(&path, binary, false)
                .unwrap()
                .is_some()
        );
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["version"], "v1");
        let hooks = written["hooks"].as_array().unwrap();
        let triggers: Vec<(&str, &str)> = hooks
            .iter()
            .map(|h| {
                (
                    h["trigger"].as_str().unwrap(),
                    h["matcher"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            triggers,
            vec![
                ("PreToolUse", "write"),
                ("PostToolUse", "write"),
                ("PreToolUse", "shell"),
                ("PostToolUse", "shell"),
            ]
        );
        for hook in hooks {
            assert_eq!(hook["action"]["type"], "command");
            assert_eq!(
                hook["action"]["command"],
                "/usr/local/bin/autter checkpoint kiro --hook-input stdin"
            );
        }
        assert_eq!(KiroInstaller::status_at(&path, binary), (true, true));
        assert!(
            KiroInstaller::install_at(&path, binary, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_moved_binary_is_out_of_date() {
        let home = tempfile::tempdir().unwrap();
        let path = KiroInstaller::hooks_path(home.path());
        KiroInstaller::install_at(&path, Path::new("/old/autter"), false).unwrap();
        assert_eq!(
            KiroInstaller::status_at(&path, Path::new("/usr/local/bin/autter")),
            (true, false)
        );
        // Reinstalling with the new binary brings it up to date.
        KiroInstaller::install_at(&path, Path::new("/usr/local/bin/autter"), false).unwrap();
        assert_eq!(
            KiroInstaller::status_at(&path, Path::new("/usr/local/bin/autter")),
            (true, true)
        );
    }

    #[test]
    fn uninstall_removes_only_autters_file() {
        let home = tempfile::tempdir().unwrap();
        let dir = KiroInstaller::hooks_dir(home.path());
        fs::create_dir_all(&dir).unwrap();
        let mine = dir.join("lint-on-save.json");
        let original = r#"{"version":"v1","hooks":[{"name":"Lint","trigger":"PostFileSave","action":{"type":"command","command":"npx eslint --fix"}}]}"#;
        fs::write(&mine, original).unwrap();
        let path = KiroInstaller::hooks_path(home.path());
        KiroInstaller::install_at(&path, Path::new("/usr/local/bin/autter"), false).unwrap();
        assert_eq!(
            fs::read_to_string(&mine).unwrap(),
            original,
            "install left it alone"
        );

        assert!(KiroInstaller::uninstall_at(&path, false).unwrap().is_some());
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(&mine).unwrap(),
            original,
            "uninstall left it alone"
        );
        assert!(KiroInstaller::uninstall_at(&path, false).unwrap().is_none());
        assert_eq!(
            KiroInstaller::status_at(&path, Path::new("/usr/local/bin/autter")),
            (false, false)
        );
    }

    #[test]
    fn dry_run_writes_nothing_and_windows_paths_use_forward_slashes() {
        let home = tempfile::tempdir().unwrap();
        let path = KiroInstaller::hooks_path(home.path());
        assert!(
            KiroInstaller::install_at(&path, Path::new("/usr/local/bin/autter"), true)
                .unwrap()
                .is_some()
        );
        assert!(!path.exists());
        let file = KiroInstaller::desired_file(Path::new(r"C:\Users\dev\.autter\bin\autter.exe"));
        assert_eq!(
            file["hooks"][0]["action"]["command"],
            "C:/Users/dev/.autter/bin/autter.exe checkpoint kiro --hook-input stdin"
        );
    }

    #[test]
    fn a_hand_edited_or_broken_file_is_rewritten_on_install() {
        let home = tempfile::tempdir().unwrap();
        let path = KiroInstaller::hooks_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(
            KiroInstaller::status_at(&path, Path::new("/usr/local/bin/autter")),
            (false, false)
        );
        KiroInstaller::install_at(&path, Path::new("/usr/local/bin/autter"), false).unwrap();
        assert_eq!(
            KiroInstaller::status_at(&path, Path::new("/usr/local/bin/autter")),
            (true, true)
        );
    }
}
