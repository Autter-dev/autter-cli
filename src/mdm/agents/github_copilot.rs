use crate::error::AutterError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{
    MIN_CODE_VERSION, generate_diff, get_editor_version, home_dir, parse_version,
    resolve_editor_cli, settings_paths_for_products, should_process_settings_target,
    version_meets_requirement, write_atomic,
};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

const GITHUB_COPILOT_PRE_TOOL_CMD: &str = "checkpoint github-copilot --hook-input stdin";
const GITHUB_COPILOT_POST_TOOL_CMD: &str = "checkpoint github-copilot --hook-input stdin";
/// Every hook event autter installs in `~/.copilot/hooks/autter.json`, which
/// both VS Code and the Copilot CLI read. `UserPromptSubmit` and `Stop` (the
/// CLI's aliases for `userPromptSubmitted` / `agentStop`) wake the transcript
/// reader when a prompt is sent or a turn ends.
const GITHUB_COPILOT_HOOK_EVENTS: [&str; 4] =
    ["PreToolUse", "PostToolUse", "UserPromptSubmit", "Stop"];
/// The events that carry the post-tool command (all but `PreToolUse`).
const GITHUB_COPILOT_SESSION_EVENTS: [&str; 2] = ["UserPromptSubmit", "Stop"];
/// `autter memory-hook github-copilot` events. Copilot drops prompt-hook
/// output, so memory comes at session start and after a file is read or
/// edited. The event is passed on the command line because the payload may
/// not name it.
const GITHUB_COPILOT_MEMORY_EVENTS: [&str; 2] = ["SessionStart", "PostToolUse"];

fn copilot_memory_hooks(binary_path: &std::path::Path) -> Vec<(&'static str, String)> {
    GITHUB_COPILOT_MEMORY_EVENTS
        .iter()
        .map(|event| {
            (
                *event,
                crate::mdm::utils::memory_hook_command(
                    &binary_path.display().to_string(),
                    "github-copilot",
                    Some(event),
                ),
            )
        })
        .collect()
}

pub struct GitHubCopilotInstaller;

impl GitHubCopilotInstaller {
    fn hooks_path() -> PathBuf {
        home_dir()
            .join(".copilot")
            .join("hooks")
            .join("autter.json")
    }

    fn legacy_hooks_path() -> PathBuf {
        home_dir().join(".github").join("hooks").join("autter.json")
    }

    fn settings_targets() -> Vec<PathBuf> {
        settings_paths_for_products(&["Code", "Code - Insiders"])
    }

    fn is_github_copilot_checkpoint_command(cmd: &str) -> bool {
        cmd.contains("autter checkpoint github-copilot")
            || (cmd.contains("autter")
                && cmd.contains("checkpoint")
                && cmd.contains("github-copilot"))
    }
}

impl HookInstaller for GitHubCopilotInstaller {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn id(&self) -> &str {
        "github-copilot"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["Code", "code"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, AutterError> {
        let resolved_cli = resolve_editor_cli("code");
        let has_cli = resolved_cli.is_some();
        let has_vscode_dotfiles = home_dir().join(".vscode").exists();
        let has_copilot_dotfiles = home_dir().join(".copilot").exists();
        let has_github_dotfiles = home_dir().join(".github").exists();
        let has_settings_targets = Self::settings_targets()
            .iter()
            .any(|path| should_process_settings_target(path));

        if !has_cli
            && !has_vscode_dotfiles
            && !has_copilot_dotfiles
            && !has_github_dotfiles
            && !has_settings_targets
        {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        // If we have a CLI, check version.
        if let Some(cli) = &resolved_cli
            && let Ok(version_str) = get_editor_version(cli)
            && let Some(version) = parse_version(&version_str)
            && !version_meets_requirement(version, MIN_CODE_VERSION)
        {
            return Err(AutterError::Generic(format!(
                "VS Code version {}.{} detected, but minimum version {}.{} is required",
                version.0, version.1, MIN_CODE_VERSION.0, MIN_CODE_VERSION.1
            )));
        }

        let hooks_path = Self::hooks_path();
        let legacy_path = Self::legacy_hooks_path();
        if !hooks_path.exists() && !legacy_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        if !hooks_path.exists() && legacy_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: true,
                hooks_up_to_date: false,
            });
        }

        let content = fs::read_to_string(&hooks_path)?;
        let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));

        let pre_desired = format!(
            "{} {}",
            params.binary_path.display(),
            GITHUB_COPILOT_PRE_TOOL_CMD
        );
        let post_desired = format!(
            "{} {}",
            params.binary_path.display(),
            GITHUB_COPILOT_POST_TOOL_CMD
        );

        let has_pre_installed = existing
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(Self::is_github_copilot_checkpoint_command)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        let has_post_installed = existing
            .get("hooks")
            .and_then(|h| h.get("PostToolUse"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(Self::is_github_copilot_checkpoint_command)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        let has_pre_up_to_date = existing
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| cmd == pre_desired)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        let has_post_up_to_date = existing
            .get("hooks")
            .and_then(|h| h.get("PostToolUse"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| cmd == post_desired)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        let has_session_up_to_date = GITHUB_COPILOT_SESSION_EVENTS.iter().all(|event| {
            existing
                .get("hooks")
                .and_then(|h| h.get(*event))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().any(|hook| {
                        hook.get("command")
                            .and_then(|c| c.as_str())
                            .map(|cmd| cmd == post_desired)
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        });

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: has_pre_installed || has_post_installed,
            hooks_up_to_date: has_pre_up_to_date
                && has_post_up_to_date
                && has_session_up_to_date
                && crate::mdm::utils::flat_memory_hooks_current(
                    &existing,
                    &copilot_memory_hooks(&params.binary_path),
                ),
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, AutterError> {
        let hooks_path = Self::hooks_path();

        if !dry_run && let Some(dir) = hooks_path.parent() {
            fs::create_dir_all(dir)?;
        }

        let existing_content = if hooks_path.exists() {
            fs::read_to_string(&hooks_path)?
        } else {
            String::new()
        };

        let existing: Value = if existing_content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&existing_content)?
        };

        let pre_tool_cmd = format!(
            "{} {}",
            params.binary_path.display(),
            GITHUB_COPILOT_PRE_TOOL_CMD
        );
        let post_tool_cmd = format!(
            "{} {}",
            params.binary_path.display(),
            GITHUB_COPILOT_POST_TOOL_CMD
        );

        let desired: Value = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "type": "command",
                        "command": pre_tool_cmd
                    }
                ],
                "PostToolUse": [
                    {
                        "type": "command",
                        "command": post_tool_cmd
                    }
                ],
                "UserPromptSubmit": [
                    {
                        "type": "command",
                        "command": post_tool_cmd
                    }
                ],
                "Stop": [
                    {
                        "type": "command",
                        "command": post_tool_cmd
                    }
                ]
            }
        });

        let mut merged = existing.clone();
        if !merged.is_object() {
            merged = json!({});
        }

        let mut hooks_obj = match merged.get("hooks") {
            Some(v) if v.is_object() => v.clone(),
            Some(_) => json!({}),
            None => json!({}),
        };

        for hook_name in &GITHUB_COPILOT_HOOK_EVENTS {
            let desired_hook = desired
                .get("hooks")
                .and_then(|h| h.get(*hook_name))
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .cloned();
            let Some(desired_hook) = desired_hook else {
                continue;
            };

            let desired_cmd = desired_hook.get("command").and_then(|c| c.as_str());
            let Some(desired_cmd) = desired_cmd else {
                continue;
            };

            let mut existing_hooks = hooks_obj
                .get(*hook_name)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let mut found_idx = None;
            let mut needs_update = false;

            for (idx, existing_hook) in existing_hooks.iter().enumerate() {
                if let Some(existing_cmd) = existing_hook.get("command").and_then(|c| c.as_str())
                    && Self::is_github_copilot_checkpoint_command(existing_cmd)
                    && found_idx.is_none()
                {
                    found_idx = Some(idx);
                    if existing_cmd != desired_cmd {
                        needs_update = true;
                    }
                }
            }

            match found_idx {
                Some(idx) => {
                    if needs_update {
                        existing_hooks[idx] = desired_hook.clone();
                    }

                    let keep_idx = idx;
                    let mut current_idx = 0;
                    existing_hooks.retain(|hook| {
                        if current_idx == keep_idx {
                            current_idx += 1;
                            true
                        } else if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                            let keep = !Self::is_github_copilot_checkpoint_command(cmd);
                            current_idx += 1;
                            keep
                        } else {
                            current_idx += 1;
                            true
                        }
                    });
                }
                None => existing_hooks.push(desired_hook.clone()),
            }

            if let Some(obj) = hooks_obj.as_object_mut() {
                obj.insert(hook_name.to_string(), Value::Array(existing_hooks));
            }
        }

        // Repository memory before code: `autter memory-hook` injects it.
        crate::mdm::utils::ensure_flat_memory_hooks(
            &mut hooks_obj,
            &copilot_memory_hooks(&params.binary_path),
            true,
        );

        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
        }

        if !dry_run {
            let legacy_path = Self::legacy_hooks_path();
            if legacy_path.exists() {
                let _ = fs::remove_file(&legacy_path);
            }
        }

        if existing == merged {
            return Ok(None);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(&hooks_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(&hooks_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, AutterError> {
        if !dry_run {
            let legacy_path = Self::legacy_hooks_path();
            if legacy_path.exists() {
                let _ = fs::remove_file(&legacy_path);
            }
        }

        let hooks_path = Self::hooks_path();

        if !hooks_path.exists() {
            return Ok(None);
        }

        let existing_content = fs::read_to_string(&hooks_path)?;
        let existing: Value = serde_json::from_str(&existing_content)?;

        let mut merged = existing.clone();
        let mut hooks_obj = match merged.get("hooks").cloned() {
            Some(h) => h,
            None => return Ok(None),
        };

        let mut changed = crate::mdm::utils::remove_flat_memory_hooks(
            &mut hooks_obj,
            &GITHUB_COPILOT_MEMORY_EVENTS,
        );

        for hook_name in &GITHUB_COPILOT_HOOK_EVENTS {
            if let Some(hooks_array) = hooks_obj.get_mut(*hook_name).and_then(|v| v.as_array_mut())
            {
                let original_len = hooks_array.len();
                hooks_array.retain(|hook| {
                    if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                        !Self::is_github_copilot_checkpoint_command(cmd)
                    } else {
                        true
                    }
                });
                if hooks_array.len() != original_len {
                    changed = true;
                }
            }
        }

        if !changed {
            return Ok(None);
        }

        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(&hooks_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(&hooks_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdm::hook_installer::HookInstaller;
    use serial_test::serial;
    use std::path::Path;
    use tempfile::tempdir;

    fn test_binary_path() -> PathBuf {
        PathBuf::from("/tmp/autter/bin/autter")
    }

    fn with_temp_home<F: FnOnce(&Path)>(f: F) {
        let temp = tempdir().unwrap();
        let home = temp.path().to_path_buf();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");

        // SAFETY: tests are serialized via #[serial], so mutating process env is safe.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("USERPROFILE", &home);
        }

        f(&home);

        // SAFETY: tests are serialized via #[serial], so restoring process env is safe.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    #[test]
    fn test_github_copilot_installer_name() {
        let installer = GitHubCopilotInstaller;
        assert_eq!(installer.name(), "GitHub Copilot");
    }

    #[test]
    fn test_github_copilot_installer_id() {
        let installer = GitHubCopilotInstaller;
        assert_eq!(installer.id(), "github-copilot");
    }

    #[test]
    #[serial]
    fn test_install_hooks_creates_expected_file() {
        with_temp_home(|home| {
            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let diff = installer.install_hooks(&params, false).unwrap();
            assert!(diff.is_some());

            let hooks_path = home.join(".copilot").join("hooks").join("autter.json");
            assert!(hooks_path.exists());

            let content: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap())
                .expect("valid json");

            let pre = content
                .get("hooks")
                .and_then(|h| h.get("PreToolUse"))
                .and_then(|v| v.as_array())
                .unwrap();
            let post = content
                .get("hooks")
                .and_then(|h| h.get("PostToolUse"))
                .and_then(|v| v.as_array())
                .unwrap();

            assert_eq!(pre.len(), 1);
            // The checkpoint and the repository memory hook.
            assert_eq!(post.len(), 2);
            assert_eq!(
                pre[0].get("command").and_then(|v| v.as_str()),
                Some("/tmp/autter/bin/autter checkpoint github-copilot --hook-input stdin")
            );
            assert_eq!(
                post[1].get("command").and_then(|v| v.as_str()),
                Some(
                    "/tmp/autter/bin/autter memory-hook github-copilot --event PostToolUse --hook-input stdin"
                )
            );
            assert_eq!(
                content["hooks"]["SessionStart"][0]["command"].as_str(),
                Some(
                    "/tmp/autter/bin/autter memory-hook github-copilot --event SessionStart --hook-input stdin"
                )
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_idempotent() {
        with_temp_home(|_| {
            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let first = installer.install_hooks(&params, false).unwrap();
            assert!(first.is_some());

            let second = installer.install_hooks(&params, false).unwrap();
            assert!(second.is_none());
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_deletes_legacy_hooks_file() {
        with_temp_home(|home| {
            let legacy_path = home.join(".github").join("hooks").join("autter.json");
            fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
            fs::write(&legacy_path, r#"{"hooks":{}}"#).unwrap();
            assert!(legacy_path.exists());

            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer.install_hooks(&params, false).unwrap();

            assert!(!legacy_path.exists());
            let new_path = home.join(".copilot").join("hooks").join("autter.json");
            assert!(new_path.exists());
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_dry_run_does_not_create_files() {
        with_temp_home(|home| {
            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let hooks_dir = home.join(".copilot").join("hooks");
            let hooks_path = hooks_dir.join("autter.json");
            assert!(!hooks_dir.exists());
            assert!(!hooks_path.exists());

            let diff = installer.install_hooks(&params, true).unwrap();
            assert!(diff.is_some());
            assert!(!hooks_dir.exists());
            assert!(!hooks_path.exists());
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_repairs_non_object_hooks_field() {
        with_temp_home(|home| {
            let hooks_path = home.join(".copilot").join("hooks").join("autter.json");
            fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
            fs::write(&hooks_path, r#"{"hooks":"invalid","extra":"keep"}"#).unwrap();

            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let diff = installer.install_hooks(&params, false).unwrap();
            assert!(diff.is_some());

            let content: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap())
                .expect("valid json");
            assert_eq!(content.get("extra").and_then(|v| v.as_str()), Some("keep"));

            let pre = content
                .get("hooks")
                .and_then(|h| h.get("PreToolUse"))
                .and_then(|v| v.as_array())
                .unwrap();
            let post = content
                .get("hooks")
                .and_then(|h| h.get("PostToolUse"))
                .and_then(|v| v.as_array())
                .unwrap();

            assert_eq!(pre.len(), 1);
            assert_eq!(post.len(), 2);
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_repairs_non_object_root() {
        with_temp_home(|home| {
            let hooks_path = home.join(".copilot").join("hooks").join("autter.json");
            fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
            fs::write(&hooks_path, "[]").unwrap();

            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let diff = installer.install_hooks(&params, false).unwrap();
            assert!(diff.is_some());

            let content: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap())
                .expect("valid json");
            let hooks = content.get("hooks").and_then(|v| v.as_object());
            assert!(hooks.is_some());
            assert!(hooks.unwrap().contains_key("PreToolUse"));
            assert!(hooks.unwrap().contains_key("PostToolUse"));
        });
    }

    #[test]
    #[serial]
    fn test_check_hooks_partial_pre_tool_use_counts_as_installed() {
        with_temp_home(|home| {
            let hooks_path = home.join(".copilot").join("hooks").join("autter.json");
            fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
            let existing = json!({
                "hooks": {
                    "PreToolUse": [
                        {
                            "type": "command",
                            "command": "/tmp/autter/bin/autter checkpoint github-copilot --hook-input stdin"
                        }
                    ]
                }
            });
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let result = installer.check_hooks(&params).unwrap();
            assert!(result.tool_installed);
            assert!(result.hooks_installed);
            assert!(!result.hooks_up_to_date);
        });
    }

    #[test]
    #[serial]
    fn test_check_hooks_partial_post_tool_use_counts_as_installed() {
        with_temp_home(|home| {
            let hooks_path = home.join(".copilot").join("hooks").join("autter.json");
            fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
            let existing = json!({
                "hooks": {
                    "PostToolUse": [
                        {
                            "type": "command",
                            "command": "/tmp/autter/bin/autter checkpoint github-copilot --hook-input stdin"
                        }
                    ]
                }
            });
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let result = installer.check_hooks(&params).unwrap();
            assert!(result.tool_installed);
            assert!(result.hooks_installed);
            assert!(!result.hooks_up_to_date);
        });
    }

    #[test]
    #[serial]
    fn test_uninstall_hooks_removes_only_autter_entries() {
        with_temp_home(|home| {
            let hooks_path = home.join(".copilot").join("hooks").join("autter.json");
            fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
            let existing = json!({
                "hooks": {
                    "PreToolUse": [
                        {
                            "type": "command",
                            "command": "echo before"
                        },
                        {
                            "type": "command",
                            "command": "/tmp/autter/bin/autter checkpoint github-copilot --hook-input stdin"
                        }
                    ],
                    "PostToolUse": [
                        {
                            "type": "command",
                            "command": "/tmp/autter/bin/autter checkpoint github-copilot --hook-input stdin"
                        }
                    ]
                }
            });
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };
            let diff = installer.uninstall_hooks(&params, false).unwrap();
            assert!(diff.is_some());

            let content: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap())
                .expect("valid json");
            let pre = content
                .get("hooks")
                .and_then(|h| h.get("PreToolUse"))
                .and_then(|v| v.as_array())
                .unwrap();
            let post = content
                .get("hooks")
                .and_then(|h| h.get("PostToolUse"))
                .and_then(|v| v.as_array())
                .unwrap();

            assert_eq!(pre.len(), 1);
            assert_eq!(
                pre[0].get("command").and_then(|v| v.as_str()),
                Some("echo before")
            );
            assert!(post.is_empty());
        });
    }

    #[test]
    #[serial]
    fn test_uninstall_hooks_deletes_legacy_hooks_file() {
        with_temp_home(|home| {
            let legacy_path = home.join(".github").join("hooks").join("autter.json");
            fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
            fs::write(&legacy_path, r#"{"hooks":{}}"#).unwrap();

            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer.uninstall_hooks(&params, false).unwrap();
            assert!(!legacy_path.exists());
        });
    }

    #[test]
    #[serial]
    fn test_check_hooks_detects_legacy_path_as_installed() {
        with_temp_home(|home| {
            let legacy_path = home.join(".github").join("hooks").join("autter.json");
            fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
            let existing = json!({
                "hooks": {
                    "PreToolUse": [
                        {
                            "type": "command",
                            "command": "/tmp/autter/bin/autter checkpoint github-copilot --hook-input stdin"
                        }
                    ]
                }
            });
            fs::write(
                &legacy_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let result = installer.check_hooks(&params).unwrap();
            assert!(result.tool_installed);
            assert!(result.hooks_installed);
            assert!(!result.hooks_up_to_date);
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_adds_prompt_and_stop_hooks_and_uninstall_removes_them() {
        with_temp_home(|home| {
            let installer = GitHubCopilotInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };
            let hooks_path = home.join(".copilot").join("hooks").join("autter.json");
            // An install from before the session hooks, plus a user Stop hook.
            fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
            let cmd = "/tmp/autter/bin/autter checkpoint github-copilot --hook-input stdin";
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&json!({
                    "hooks": {
                        "PreToolUse": [{"type": "command", "command": cmd}],
                        "PostToolUse": [{"type": "command", "command": cmd}],
                        "Stop": [{"type": "command", "command": "say done"}]
                    }
                }))
                .unwrap(),
            )
            .unwrap();

            assert!(installer.install_hooks(&params, false).unwrap().is_some());
            let content: Value =
                serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
            for event in GITHUB_COPILOT_SESSION_EVENTS {
                let commands: Vec<&str> = content["hooks"][event]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|h| h["command"].as_str())
                    .collect();
                assert!(commands.contains(&cmd), "{event}: {commands:?}");
            }
            let stop: Vec<&str> = content["hooks"]["Stop"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|h| h["command"].as_str())
                .collect();
            assert!(stop.contains(&"say done"), "the user's Stop hook is kept");
            assert!(installer.install_hooks(&params, false).unwrap().is_none());

            installer.uninstall_hooks(&params, false).unwrap();
            let content: Value =
                serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
            for event in GITHUB_COPILOT_HOOK_EVENTS {
                let ours = content["hooks"][event]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|h| h["command"].as_str())
                            .any(GitHubCopilotInstaller::is_github_copilot_checkpoint_command)
                    })
                    .unwrap_or(false);
                assert!(!ours, "{event}");
            }
        });
    }
}
