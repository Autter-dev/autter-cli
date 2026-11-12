//! `autter doctor --agent <name>`: verify one agent's integration end to end.
//!
//! For the named agent: is it detected, which files does autter manage to
//! hook it, does each hook command run, and does a simulated edit (the
//! agent's own pre- and post-edit hook payloads, piped into `autter
//! checkpoint <preset> --hook-input stdin` in a throwaway repository) reach
//! the background service as an AI checkpoint. Verifying an integration used
//! to need a real agent session.

use super::{DoctorCheck, DoctorOutput, DoctorStatus, Reporter, skipped_check, version_string};
use crate::mdm::hook_installer::{HookInstaller, HookInstallerParams};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(super) const SECTION_AGENT: &str = "Agent integration";
pub(super) const SECTION_SIMULATION: &str = "Simulated hook calls";

/// The file the simulated edit writes, relative to the throwaway repository.
const SIM_FILE: &str = "agent-check.txt";
const SIM_BEFORE: &str = "written before the agent's edit\n";
const SIM_AFTER: &str = "written before the agent's edit\nwritten by the simulated agent edit\n";
/// How long one simulated hook call may take, before scaling for slow machines.
const HOOK_CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the background service may take to persist a checkpoint.
const PERSIST_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Integration {
    /// Hook commands in the agent's own config file.
    CliHooks,
    /// A plugin file the agent loads, which runs `autter checkpoint`.
    Plugin,
    /// An editor extension or IDE plugin; there is no hook to simulate.
    IdeExtension,
}

impl Integration {
    fn label(self) -> &'static str {
        match self {
            Integration::CliHooks => "CLI hooks",
            Integration::Plugin => "plugin",
            Integration::IdeExtension => "IDE extension",
        }
    }
}

pub(super) fn integration_for(id: &str) -> Integration {
    match id {
        "amp" | "opencode" | "pi" => Integration::Plugin,
        "vscode" | "jetbrains" | "visual-studio" => Integration::IdeExtension,
        _ => Integration::CliHooks,
    }
}

/// Other names people use for an agent, mapped to its installer id.
const ALIASES: &[(&str, &str)] = &[
    ("claude", "claude-code"),
    ("claude-code-cli", "claude-code"),
    ("copilot", "github-copilot"),
    ("copilot-cli", "github-copilot"),
    ("github-copilot-cli", "github-copilot"),
    ("code", "vscode"),
    ("vs-code", "vscode"),
    ("gemini-cli", "gemini"),
    ("factory", "droid"),
    ("factory-droid", "droid"),
    ("agy", "antigravity"),
    ("google-antigravity", "antigravity"),
    ("antigravity-pre", "antigravity"),
    ("antigravity-post", "antigravity"),
    ("intellij", "jetbrains"),
    ("jetbrains-ides", "jetbrains"),
    ("visualstudio", "visual-studio"),
];

fn normalize(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace([' ', '_'], "-")
}

/// The installer id `name` refers to, among `known` installer `(id, name)`s.
pub(super) fn resolve_agent_id(name: &str, known: &[(String, String)]) -> Option<String> {
    let wanted = normalize(name);
    let wanted = ALIASES
        .iter()
        .find(|(alias, _)| *alias == wanted)
        .map(|(_, id)| id.to_string())
        .unwrap_or(wanted);
    known
        .iter()
        .find(|(id, display)| *id == wanted || normalize(display) == wanted)
        .map(|(id, _)| id.clone())
}

/// The presets an agent's hooks run for a pre-edit and a post-edit call, and
/// a representative payload for each, in the shapes the presets parse.
/// `None` for editor extensions.
pub(super) struct Simulation {
    pub pre_preset: &'static str,
    pub post_preset: &'static str,
    pub pre: Value,
    pub post: Value,
    /// The pre-edit call names no file (Codex's PreToolUse carries no paths),
    /// so it records nothing for the file by itself.
    pub pre_names_file: bool,
}

pub(super) fn simulation_for(
    id: &str,
    cwd: &Path,
    file: &Path,
    transcript: &Path,
) -> Option<Simulation> {
    let cwd = cwd.to_string_lossy().to_string();
    let file = file.to_string_lossy().to_string();
    let transcript = transcript.to_string_lossy().to_string();
    let session = "autter-doctor-simulation";
    let tool_use = "autter_doctor_tool_use";
    let same = |preset: &'static str, pre: Value, post: Value| Simulation {
        pre_preset: preset,
        post_preset: preset,
        pre,
        post,
        pre_names_file: true,
    };
    let sim = match id {
        "claude-code" => {
            let call = |event: &str| {
                json!({"cwd": cwd, "transcript_path": transcript, "session_id": session,
                       "hook_event_name": event, "tool_name": "Write", "tool_use_id": tool_use,
                       "tool_input": {"file_path": file, "content": SIM_AFTER}})
            };
            same("claude", call("PreToolUse"), call("PostToolUse"))
        }
        "codex" => {
            let call = |event: &str| {
                json!({"cwd": cwd, "session_id": session, "hook_event_name": event,
                       "tool_name": "apply_patch", "tool_use_id": tool_use,
                       "transcript_path": transcript,
                       "tool_input": {"patch": format!("*** Begin Patch\n*** Update File: {file}\n*** End Patch\n")}})
            };
            Simulation {
                pre_names_file: false,
                ..same("codex", call("PreToolUse"), call("PostToolUse"))
            }
        }
        "cursor" => {
            let call = |event: &str| {
                json!({"conversation_id": session, "workspace_roots": [cwd],
                       "hook_event_name": event, "tool_name": "Write",
                       "tool_input": {"file_path": file}})
            };
            same("cursor", call("preToolUse"), call("postToolUse"))
        }
        "github-copilot" => {
            let call = |event: &str| {
                json!({"hook_event_name": event, "cwd": cwd, "session_id": session,
                       "tool_name": "replace_string_in_file", "tool_use_id": tool_use,
                       "tool_input": {"filePath": file}})
            };
            same("github-copilot", call("PreToolUse"), call("PostToolUse"))
        }
        "amp" => {
            let call = |event: &str| {
                json!({"hook_event_name": event, "tool_name": "Edit", "thread_id": session,
                       "tool_use_id": tool_use, "cwd": cwd, "tool_input": {"path": file}})
            };
            same("amp", call("PreToolUse"), call("PostToolUse"))
        }
        "opencode" => {
            let call = |event: &str| {
                json!({"hook_event_name": event, "session_id": session, "cwd": cwd,
                       "tool_name": "edit", "tool_use_id": tool_use,
                       "tool_input": {"file_path": file}})
            };
            same("opencode", call("PreToolUse"), call("PostToolUse"))
        }
        "pi" => same(
            "pi",
            json!({"hook_event_name": "before_edit", "session_id": session,
                   "session_path": transcript, "cwd": cwd, "tool_name": "edit",
                   "will_edit_filepaths": [file]}),
            json!({"hook_event_name": "after_edit", "session_id": session,
                   "session_path": transcript, "cwd": cwd, "tool_name": "edit",
                   "edited_filepaths": [file]}),
        ),
        "gemini" => {
            let call = |event: &str| {
                json!({"cwd": cwd, "session_id": session, "transcript_path": transcript,
                       "hook_event_name": event, "tool_name": "write_file",
                       "tool_use_id": tool_use, "tool_input": {"file_path": file}})
            };
            same("gemini", call("BeforeTool"), call("AfterTool"))
        }
        "droid" => {
            let call = |event: &str| {
                json!({"cwd": cwd, "hookEventName": event, "session_id": session,
                       "tool_name": "Edit", "tool_use_id": tool_use,
                       "transcript_path": transcript, "tool_input": {"file_path": file}})
            };
            same("droid", call("PreToolUse"), call("PostToolUse"))
        }
        "firebender" => {
            let call = |event: &str| {
                json!({"hook_event_name": event, "model": "unknown",
                       "conversation_id": session, "repo_working_dir": cwd,
                       "tool_name": "Write", "tool_use_id": tool_use,
                       "tool_input": {"file_path": file}})
            };
            same("firebender", call("preToolUse"), call("postToolUse"))
        }
        "antigravity" => {
            let call = json!({"toolCall": {"name": "write_to_file", "args": {"TargetFile": file}},
                              "stepIdx": 1, "conversationId": session,
                              "workspacePaths": [cwd]});
            Simulation {
                pre_preset: "antigravity-pre",
                post_preset: "antigravity-post",
                pre: call.clone(),
                post: call,
                pre_names_file: true,
            }
        }
        "windsurf" => {
            let call = |action: &str| {
                json!({"trajectory_id": session, "agent_action_name": action,
                       "tool_info": {"cwd": cwd, "file_path": file}})
            };
            same("windsurf", call("pre_write_code"), call("post_write_code"))
        }
        _ => return None,
    };
    Some(sim)
}

/// The binary an agent's hook command runs (the part before ` checkpoint `),
/// when it names one that exists.
fn hook_binary(command: &str) -> Option<PathBuf> {
    let at = command.find(" checkpoint ")?;
    let binary = command[..at].trim().trim_matches(['"', '\'']);
    let path = PathBuf::from(binary);
    path.is_file().then_some(path)
}

pub(super) fn known_agents() -> Vec<(String, String)> {
    crate::mdm::agents::get_all_installers()
        .iter()
        .map(|i| (i.id().to_string(), i.name().to_string()))
        .collect()
}

/// Run every check for the agent `installer` and report them.
pub(super) fn run(installer: &dyn HookInstaller, json: bool) -> DoctorOutput {
    let mut reporter = Reporter::new(json);
    let id = installer.id().to_string();
    let name = installer.name().to_string();
    if !json {
        println!("autter doctor --agent {id} (autter {})", version_string());
        println!(
            "Checking {name}: detection, hook files, hook commands, then a simulated edit through its hooks."
        );
    }
    let integration = integration_for(&id);

    let binary_path = crate::mdm::utils::resolve_hook_binary_path()
        .or_else(|_| std::env::current_exe())
        .unwrap_or_else(|_| PathBuf::from("autter"));
    let params = HookInstallerParams { binary_path };

    // 1. Detected?
    reporter.begin(SECTION_AGENT, "looking for the agent");
    let status = installer.check_hooks(&params);
    let detected = matches!(&status, Ok(r) if r.tool_installed);
    reporter.add(match &status {
        Ok(r) if r.tool_installed => DoctorCheck {
            section: SECTION_AGENT,
            name: "detected".to_string(),
            status: DoctorStatus::Passed,
            summary: format!("{name} is installed on this machine ({})", integration.label()),
            details: Vec::new(),
            remediation: None,
        },
        Ok(_) => DoctorCheck {
            section: SECTION_AGENT,
            name: "detected".to_string(),
            status: DoctorStatus::Warning,
            summary: format!("{name} was not found on this machine"),
            details: vec![
                "the simulated hook calls below still check autter's side".to_string(),
            ],
            remediation: Some(format!(
                "install {name} on this machine (where your agents run), then run `autter install-hooks`"
            )),
        },
        Err(err) => DoctorCheck {
            section: SECTION_AGENT,
            name: "detected".to_string(),
            status: DoctorStatus::Failed,
            summary: format!("{name} cannot be hooked: {err}"),
            details: Vec::new(),
            remediation: Some(format!(
                "update {name} to a supported version, then run `autter install-hooks`"
            )),
        },
    });

    // 2. Files autter manages for it.
    let files = installer.managed_files();
    let installed = matches!(&status, Ok(r) if r.hooks_installed);
    let up_to_date = matches!(&status, Ok(r) if r.hooks_up_to_date);
    let file_details: Vec<String> = files
        .iter()
        .map(|path| {
            if path.is_file() {
                let count =
                    crate::mdm::hook_refresh::autter_commands_in_files(std::slice::from_ref(path))
                        .len();
                if count > 0 {
                    format!("{} ({count} autter hook commands)", path.display())
                } else {
                    format!("{} (exists)", path.display())
                }
            } else {
                format!("{} (not there)", path.display())
            }
        })
        .collect();
    let what = if integration == Integration::IdeExtension {
        "the autter extension"
    } else {
        "autter hooks"
    };
    reporter.add(if installed && up_to_date {
        DoctorCheck {
            section: SECTION_AGENT,
            name: "hook files".to_string(),
            status: DoctorStatus::Passed,
            summary: format!("{what} installed (up to date)"),
            details: file_details,
            remediation: None,
        }
    } else if installed {
        DoctorCheck {
            section: SECTION_AGENT,
            name: "hook files".to_string(),
            status: DoctorStatus::Warning,
            summary: format!("{what} installed but out of date"),
            details: file_details,
            remediation: Some(format!("run `autter install-hooks`, then restart {name}")),
        }
    } else if !detected {
        DoctorCheck {
            section: SECTION_AGENT,
            name: "hook files".to_string(),
            status: DoctorStatus::Skipped,
            summary: format!("not installed: {name} is not on this machine"),
            details: file_details,
            remediation: None,
        }
    } else {
        DoctorCheck {
            section: SECTION_AGENT,
            name: "hook files".to_string(),
            status: DoctorStatus::Failed,
            summary: format!("{what} NOT installed -- {name}'s edits are not captured"),
            details: file_details,
            remediation: Some(format!("run `autter install-hooks`, then restart {name}")),
        }
    });

    // 3. Does each hook command run?
    let commands = crate::mdm::hook_refresh::autter_commands_in_files(&files);
    let problems: Vec<(String, String)> = commands
        .iter()
        .filter_map(|cmd| {
            crate::mdm::hook_refresh::command_problem(cmd).map(|why| (why, cmd.clone()))
        })
        .collect();
    reporter.add(if commands.is_empty() {
        skipped_check(
            SECTION_AGENT,
            "hook commands",
            match integration {
                Integration::IdeExtension => "none: the extension calls autter itself",
                Integration::Plugin => "none to list: the plugin file calls autter itself",
                Integration::CliHooks => "none found in the hook files",
            },
        )
    } else if let Some((why, _)) = problems.first() {
        DoctorCheck {
            section: SECTION_AGENT,
            name: "hook commands".to_string(),
            status: DoctorStatus::Failed,
            summary: format!(
                "{} of {} hook commands fail when {name} runs them: {why}",
                problems.len(),
                commands.len()
            ),
            details: problems
                .iter()
                .map(|(why, cmd)| format!("fails because {why}: {cmd}"))
                .collect(),
            remediation: Some(format!(
                "run `autter install-hooks` to rewrite them, then restart {name}"
            )),
        }
    } else {
        DoctorCheck {
            section: SECTION_AGENT,
            name: "hook commands".to_string(),
            status: DoctorStatus::Passed,
            summary: format!("all {} hook commands run", commands.len()),
            details: commands.iter().map(|c| format!("runs: {c}")).collect(),
            remediation: None,
        }
    });

    // 4. Simulated pre- and post-edit hook calls.
    if integration == Integration::IdeExtension {
        reporter.add(skipped_check(
            SECTION_SIMULATION,
            "simulated edit",
            format!(
                "{name} is captured by its extension, so there is no hook call to simulate: edit a file in {name}, then run `autter status`"
            ),
        ));
    } else {
        let binary = commands
            .iter()
            .filter(|c| crate::mdm::hook_refresh::command_problem(c).is_none())
            .find_map(|c| hook_binary(c))
            .or_else(|| std::env::current_exe().ok());
        simulate(&mut reporter, &id, &name, binary);
    }

    reporter.finish()
}

fn simulate(reporter: &mut Reporter, id: &str, name: &str, binary: Option<PathBuf>) {
    reporter.begin(SECTION_SIMULATION, "checking the background service");
    let service = super::check_from_diagnostic(
        SECTION_SIMULATION,
        "background service",
        crate::diagnostics::ensure_daemon_ready_for_self_checks(),
    );
    let service_status = service.status;
    reporter.add(service);
    if service_status != DoctorStatus::Passed {
        let why = if service_status == DoctorStatus::Skipped {
            "skipped -- the background service starts after `autter onboard`"
        } else {
            "skipped -- fix the background service check first"
        };
        reporter.add(skipped_check(SECTION_SIMULATION, "pre-edit hook", why));
        reporter.add(skipped_check(SECTION_SIMULATION, "post-edit hook", why));
        return;
    }
    let Some(binary) = binary else {
        reporter.add(DoctorCheck {
            section: SECTION_SIMULATION,
            name: "pre-edit hook".to_string(),
            status: DoctorStatus::Failed,
            summary: "could not find the autter binary to run".to_string(),
            details: Vec::new(),
            remediation: Some("run `autter install-hooks`, then re-run this check".to_string()),
        });
        return;
    };

    let root = crate::diagnostic_sentinels::debug_self_check_root();
    let run_id = crate::uuid::generate_v4();
    let repo = root.join(format!("agent-{id}-{run_id}"));
    let transcript = root.join(format!("agent-{id}-{run_id}.jsonl"));
    let file = repo.join(SIM_FILE);
    let rerun = format!("autter doctor --agent {id}");

    reporter.begin(SECTION_SIMULATION, "creating a throwaway repository");
    if let Err(err) = init_repo(&repo).and_then(|()| {
        std::fs::write(&file, SIM_BEFORE).map_err(|e| format!("could not write {SIM_FILE}: {e}"))
    }) {
        reporter.add(DoctorCheck {
            section: SECTION_SIMULATION,
            name: "pre-edit hook".to_string(),
            status: DoctorStatus::Failed,
            summary: format!("could not set up the test repository: {err}"),
            details: vec![format!("repository: {}", repo.display())],
            remediation: Some(format!(
                "check that {} is writable, then run `{rerun}`",
                root.display()
            )),
        });
        return;
    }
    // Canonical paths: hook payloads carry the paths the agent sees.
    let repo = std::fs::canonicalize(&repo).unwrap_or(repo);
    let file = repo.join(SIM_FILE);
    let Some(sim) = simulation_for(id, &repo, &file, &transcript) else {
        return;
    };
    let deadline_for = |base: Duration| Instant::now() + crate::diagnostics::check_timeout(base);
    let mut keep_repo = false;

    reporter.begin(
        SECTION_SIMULATION,
        &format!("sending {name}'s pre-edit hook call"),
    );
    let pre = run_hook_call(&binary, &repo, sim.pre_preset, &sim.pre);
    let pre_seen = pre.is_ok()
        && (!sim.pre_names_file
            || wait_for_checkpoint(&repo, deadline_for(PERSIST_TIMEOUT), |c| {
                !c.kind.is_ai() && c.entries.iter().any(|e| e.file == SIM_FILE)
            })
            .is_some());
    let command = format!(
        "{} checkpoint {} --hook-input stdin",
        binary.display(),
        sim.pre_preset
    );
    reporter.add(match &pre {
        Ok(_) if pre_seen => DoctorCheck {
            section: SECTION_SIMULATION,
            name: "pre-edit hook".to_string(),
            status: DoctorStatus::Passed,
            summary: if sim.pre_names_file {
                format!("`checkpoint {}` recorded the file before the edit", sim.pre_preset)
            } else {
                format!(
                    "`checkpoint {}` accepted the call ({name}'s pre-edit call names no file)",
                    sim.pre_preset
                )
            },
            details: vec![format!("ran: {command}")],
            remediation: None,
        },
        Ok(stderr) | Err(stderr) => {
            keep_repo = true;
            DoctorCheck {
                section: SECTION_SIMULATION,
                name: "pre-edit hook".to_string(),
                status: DoctorStatus::Failed,
                summary: "the pre-edit hook call was not recorded".to_string(),
                details: vec![format!("ran: {command}"), format!("output: {stderr}")],
                remediation: Some(format!(
                    "run `autter bg restart`, then `{rerun}`; if it still fails, run `autter doctor` for the full checks"
                )),
            }
        }
    });

    reporter.begin(
        SECTION_SIMULATION,
        &format!("sending {name}'s post-edit hook call"),
    );
    let _ = std::fs::write(&file, SIM_AFTER);
    let post = run_hook_call(&binary, &repo, sim.post_preset, &sim.post);
    let ai = post.is_ok().then(|| {
        wait_for_checkpoint(&repo, deadline_for(PERSIST_TIMEOUT), |c| {
            c.kind.is_ai() && c.entries.iter().any(|e| e.file == SIM_FILE)
        })
    });
    let command = format!(
        "{} checkpoint {} --hook-input stdin",
        binary.display(),
        sim.post_preset
    );
    reporter.add(match (&post, ai.flatten()) {
        (Ok(_), Some(checkpoint)) => {
            let agent = checkpoint
                .agent_id
                .as_ref()
                .map(|a| format!("tool {}, model {}", a.tool, a.model))
                .unwrap_or_else(|| "no agent recorded".to_string());
            DoctorCheck {
                section: SECTION_SIMULATION,
                name: "post-edit hook".to_string(),
                status: DoctorStatus::Passed,
                summary: format!(
                    "the background service recorded an AI checkpoint for the edited file ({agent})"
                ),
                details: vec![format!("ran: {command}")],
                remediation: None,
            }
        }
        (Ok(stderr), None) | (Err(stderr), _) => {
            keep_repo = true;
            DoctorCheck {
                section: SECTION_SIMULATION,
                name: "post-edit hook".to_string(),
                status: DoctorStatus::Failed,
                summary: "no AI checkpoint was recorded for the edited file".to_string(),
                details: vec![
                    format!("ran: {command}"),
                    format!("output: {}", if stderr.is_empty() { "(none)" } else { stderr }),
                    format!("test repository kept for inspection: {}", repo.display()),
                ],
                remediation: Some(format!(
                    "run `autter bg restart`, then `{rerun}`; if it still fails, run `autter doctor` for the full checks"
                )),
            }
        }
    });

    crate::diagnostics::wait_for_daemon_family_idle(&repo, deadline_for(PERSIST_TIMEOUT));
    if !keep_repo {
        let _ = std::fs::remove_dir_all(&repo);
    }
}

fn init_repo(repo: &Path) -> Result<(), String> {
    std::fs::create_dir_all(repo).map_err(|e| format!("could not create it: {e}"))?;
    let git = crate::config::Config::get().git_cmd().to_string();
    for args in [
        vec!["init", "-q", "."],
        vec!["config", "user.name", "Autter Doctor"],
        vec!["config", "user.email", "doctor-agent-check@autter.invalid"],
        // The sentinel remote keeps this repository out of usage metrics.
        vec![
            "remote",
            "add",
            "origin",
            crate::diagnostic_sentinels::DEBUG_SELF_CHECK_REMOTE_URL,
        ],
    ] {
        let output = Command::new(&git)
            .args(&args)
            .current_dir(repo)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("could not run git: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(())
}

/// Pipe `payload` into `<binary> checkpoint <preset> --hook-input stdin` in
/// `repo`, the way the agent's hook does. `Ok(stderr)` when it exited 0.
fn run_hook_call(
    binary: &Path,
    repo: &Path,
    preset: &str,
    payload: &Value,
) -> Result<String, String> {
    let stderr_path = repo.with_extension(format!("{preset}.stderr"));
    let stderr_file = std::fs::File::create(&stderr_path)
        .map_err(|e| format!("could not create {}: {e}", stderr_path.display()))?;
    let missing = repo.join(".no-such-agent-store");
    let mut child = Command::new(binary)
        .args(["checkpoint", preset, "--hook-input", "stdin"])
        .current_dir(repo)
        // Never mistaken for the agent's own hook runs, and never reads the
        // user's real agent stores for this throwaway session.
        .env(
            crate::commands::checkpoint_agent::hook_heartbeat::SIMULATED_HOOK_ENV,
            "1",
        )
        .env("AUTTER_AMP_THREADS_PATH", &missing)
        .env("AUTTER_OPENCODE_STORAGE_PATH", &missing)
        .env_remove("AUTTER")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", binary.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.to_string().as_bytes());
    }
    let deadline = Instant::now() + crate::diagnostics::check_timeout(HOOK_CALL_TIMEOUT);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stderr = std::fs::read_to_string(&stderr_path)
        .unwrap_or_default()
        .trim()
        .to_string();
    let _ = std::fs::remove_file(&stderr_path);
    match status {
        Some(s) if s.success() => Ok(stderr),
        Some(s) => Err(format!("exit {}: {stderr}", s.code().unwrap_or(-1))),
        None => Err(format!(
            "timed out after {:.0}s: {stderr}",
            crate::diagnostics::check_timeout(HOOK_CALL_TIMEOUT).as_secs_f64()
        )),
    }
}

/// The first persisted checkpoint matching `pred`, polling until `deadline`.
fn wait_for_checkpoint(
    repo: &Path,
    deadline: Instant,
    pred: impl Fn(&crate::authorship::working_log::Checkpoint) -> bool,
) -> Option<crate::authorship::working_log::Checkpoint> {
    loop {
        if let Ok(repository) =
            crate::git::repository::discover_repository_in_path_no_git_exec(repo)
            && let Ok(log) = repository.storage.working_log_for_base_commit("initial")
            && let Ok(checkpoints) = log.read_all_checkpoints()
            && let Some(found) = checkpoints.into_iter().find(|c| pred(c))
        {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};

    fn known() -> Vec<(String, String)> {
        known_agents()
    }

    #[test]
    fn agent_names_resolve_by_id_name_preset_and_alias() {
        let k = known();
        for (input, id) in [
            ("claude-code", "claude-code"),
            ("claude", "claude-code"),
            ("Claude Code", "claude-code"),
            ("copilot", "github-copilot"),
            ("GitHub Copilot", "github-copilot"),
            ("codex", "codex"),
            ("antigravity-pre", "antigravity"),
            ("Google Antigravity", "antigravity"),
            ("JetBrains IDEs", "jetbrains"),
            ("vscode", "vscode"),
            ("windsurf", "windsurf"),
        ] {
            assert_eq!(resolve_agent_id(input, &k).as_deref(), Some(id), "{input}");
        }
        assert_eq!(resolve_agent_id("notepad", &k), None);
    }

    /// Every agent with hooks has a simulation, and its payloads parse with
    /// the agent's own preset into a pre-edit then a post-edit of the file.
    /// A new agent without one fails here.
    #[test]
    fn every_hooked_agent_has_payloads_its_preset_parses() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let file = cwd.join(SIM_FILE);
        let transcript = cwd.join("missing.jsonl");
        // Hermetic: these presets would otherwise look in the user's stores.
        let missing = cwd.join("missing-store");
        // SAFETY: tests in this module only read these variables.
        unsafe {
            std::env::set_var("AUTTER_AMP_THREADS_PATH", &missing);
            std::env::set_var("AUTTER_OPENCODE_STORAGE_PATH", &missing);
        }
        for (id, _) in known() {
            let sim = simulation_for(&id, &cwd, &file, &transcript);
            if integration_for(&id) == Integration::IdeExtension {
                assert!(sim.is_none(), "{id}: extensions have no hook call");
                continue;
            }
            let sim = sim.unwrap_or_else(|| panic!("{id}: no simulated hook calls"));

            let pre = resolve_preset(sim.pre_preset)
                .unwrap()
                .parse(&sim.pre.to_string(), "t_doctor000000001")
                .unwrap_or_else(|e| panic!("{id} pre: {e}"));
            match pre.as_slice() {
                [ParsedHookEvent::PreFileEdit(e)] => {
                    if sim.pre_names_file {
                        assert!(e.file_paths.contains(&file), "{id} pre: {:?}", e.file_paths);
                    }
                }
                other => panic!("{id} pre: expected one PreFileEdit, got {other:?}"),
            }

            let post = resolve_preset(sim.post_preset)
                .unwrap()
                .parse(&sim.post.to_string(), "t_doctor000000002")
                .unwrap_or_else(|e| panic!("{id} post: {e}"));
            match post.as_slice() {
                [ParsedHookEvent::PostFileEdit(e)] => {
                    assert!(
                        e.file_paths.contains(&file),
                        "{id} post: {:?}",
                        e.file_paths
                    )
                }
                other => panic!("{id} post: expected one PostFileEdit, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_hook_binary_is_read_from_the_hook_command() {
        let exe = std::env::current_exe().unwrap();
        let cmd = format!("{} checkpoint claude --hook-input stdin", exe.display());
        assert_eq!(hook_binary(&cmd), Some(exe.clone()));
        let quoted = format!("\"{}\" checkpoint codex --hook-input stdin", exe.display());
        assert_eq!(hook_binary(&quoted), Some(exe));
        assert_eq!(
            hook_binary("/no/such/autter checkpoint claude --hook-input stdin"),
            None
        );
        assert_eq!(hook_binary("autter memory-hook claude"), None);
    }

    #[test]
    fn integration_types() {
        assert_eq!(integration_for("claude-code"), Integration::CliHooks);
        assert_eq!(integration_for("opencode"), Integration::Plugin);
        assert_eq!(integration_for("jetbrains"), Integration::IdeExtension);
    }
}
