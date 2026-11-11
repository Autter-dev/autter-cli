//! A hook written by an older autter must never block the agent.
//!
//! Agents run the command in their hook config on every tool call. When that
//! command no longer matches the installed CLI (a preset renamed or removed,
//! a flag dropped, the background service restarting after an upgrade), the
//! hook must still answer the way the agent expects for "no opinion": exit 0,
//! nothing on stdout that the agent can't parse (`{}` for Antigravity, which
//! reads every hook's stdout as JSON), and one line on stderr that says the
//! hook is out of date and how to fix it.
//!
//! These replay hook commands exactly as agent config files spell them, so the
//! harness's checkpoint-argument normalization is bypassed.

use crate::repos::test_repo::TestRepo;
use serde_json::Value;
use std::io::Write;
use std::process::Stdio;

const STALE_MESSAGE: &str = "autter hook is out of date. Run `autter install-hooks`";

/// Presets autter installs as shell-command hooks, by agent. Plugin-based
/// agents (Amp, OpenCode, Pi) call autter from their own code, which already
/// catches failures, so they are not listed.
const SHELL_HOOK_PRESETS: &[&str] = &[
    "antigravity-pre",
    "antigravity-post",
    "claude",
    "codex",
    "gemini",
    "cursor",
    "windsurf",
    "droid",
    "github-copilot",
    "firebender",
];

struct HookRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_hook(repo: &TestRepo, args: &[&str], stdin: &str, envs: &[(&str, &str)]) -> HookRun {
    let mut command = repo.autter_command_verbatim(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn autter");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let output = child.wait_with_output().expect("wait for autter");
    HookRun {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

/// The agent-facing contract: exit 0, and stdout is a no-op the agent can
/// parse. Antigravity must get exactly `{}`; the others accept empty stdout
/// or a JSON object with no fields set.
fn assert_fails_open(label: &str, preset: &str, run: &HookRun) {
    assert_eq!(
        run.code,
        Some(0),
        "{label}: hook must exit 0\nstdout: {}\nstderr: {}",
        run.stdout,
        run.stderr
    );
    let out = run.stdout.trim();
    if preset.starts_with("antigravity") {
        assert_eq!(
            out, "{}",
            "{label}: Antigravity needs `{{}}` on stdout\nstderr: {}",
            run.stderr
        );
    } else {
        assert!(
            out.is_empty()
                || serde_json::from_str::<Value>(out)
                    .is_ok_and(|v| v == Value::Object(Default::default())),
            "{label}: stdout must be empty or `{{}}`, got: {out}"
        );
    }
}

#[test]
fn unknown_preset_from_an_older_hook_fails_open_with_a_fix() {
    let repo = TestRepo::new();
    // Preset names this CLI does not know: what a hook written by a CLI that
    // used a different name would send.
    for (preset, agent_kind) in [
        ("antigravity", "antigravity"),
        ("antigravity-before-tool", "antigravity"),
        ("claude-code", "claude"),
        ("copilot", "github-copilot"),
        ("gemini-cli", "gemini"),
    ] {
        let run = run_hook(
            &repo,
            &["checkpoint", preset, "--hook-input", "stdin"],
            r#"{"cwd": "/tmp"}"#,
            &[],
        );
        assert_fails_open(&format!("unknown preset {preset}"), agent_kind, &run);
        assert!(
            run.stderr.contains(STALE_MESSAGE),
            "{preset}: stderr must say the hook is stale: {}",
            run.stderr
        );
        assert!(
            run.stderr.contains(preset),
            "{preset}: stderr must name the preset: {}",
            run.stderr
        );
    }
}

#[test]
fn hook_with_a_dropped_flag_value_fails_open() {
    let repo = TestRepo::new();
    for preset in SHELL_HOOK_PRESETS {
        // `--hook-input` with no value, and with an empty one.
        for args in [
            vec!["checkpoint", preset, "--hook-input"],
            vec!["checkpoint", preset, "--hook-input", ""],
        ] {
            let run = run_hook(&repo, &args, "", &[]);
            assert_fails_open(&format!("{args:?}"), preset, &run);
            assert!(
                run.stderr.contains(STALE_MESSAGE),
                "{args:?}: stderr must say the hook is stale: {}",
                run.stderr
            );
        }
    }
}

#[test]
fn hook_fails_open_when_the_background_service_is_unreachable() {
    // Right after an upgrade the old service is stopping and the new one is
    // not up yet. Every agent's hook must still answer.
    let repo = TestRepo::new();
    for preset in SHELL_HOOK_PRESETS {
        let run = run_hook(
            &repo,
            &["checkpoint", preset, "--hook-input", "stdin"],
            r#"{"cwd": "/tmp"}"#,
            &[("AUTTER_TEST_DAEMON_UNAVAILABLE", "1")],
        );
        assert_fails_open(&format!("{preset} with service down"), preset, &run);
    }
}

#[test]
fn hook_fails_open_on_garbage_input() {
    let repo = TestRepo::new();
    for preset in SHELL_HOOK_PRESETS {
        let run = run_hook(
            &repo,
            &["checkpoint", preset, "--hook-input", "stdin"],
            "not json at all",
            &[],
        );
        assert_fails_open(&format!("{preset} with garbage input"), preset, &run);
    }
}

#[test]
fn memory_hook_fails_open_when_the_background_service_is_unreachable() {
    let repo = TestRepo::new();
    for agent in [
        "claude",
        "codex",
        "gemini",
        "cursor",
        "droid",
        "github-copilot",
    ] {
        let run = run_hook(
            &repo,
            &["memory-hook", agent, "--hook-input", "stdin"],
            r#"{"cwd": "/tmp", "hook_event_name": "UserPromptSubmit", "prompt": "hi"}"#,
            &[("AUTTER_TEST_DAEMON_UNAVAILABLE", "1")],
        );
        assert_eq!(
            run.code,
            Some(0),
            "memory-hook {agent}: must exit 0\nstderr: {}",
            run.stderr
        );
    }
}

/// Run autter with every home-like location pinned to the test's own home,
/// so installers and the refresh worker it starts never read or write the
/// real user's agent configs (on Windows they resolve home from USERPROFILE,
/// which the harness leaves alone).
fn run_in_test_home(repo: &TestRepo, args: &[&str], envs: &[(&str, &str)]) -> HookRun {
    let home = repo.test_home_path();
    let mut command = repo.autter_command_verbatim(args);
    command
        .env("USERPROFILE", home)
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env_remove("HOMEDRIVE")
        .env_remove("HOMEPATH")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME");
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.stdin(Stdio::null()).output().expect("run autter");
    HookRun {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

fn doctor_check(repo: &TestRepo, name: &str) -> Value {
    let run = run_in_test_home(repo, &["doctor", "--json"], &[]);
    let report: Value = run
        .stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .next_back()
        .unwrap_or_else(|| panic!("no doctor JSON:\n{}\n{}", run.stdout, run.stderr));
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no '{name}' check in:\n{report}"))
}

#[test]
fn first_command_after_upgrade_refreshes_only_consented_hooks() {
    let repo = TestRepo::new();
    let home = repo.test_home_path().clone();
    let stale_cmd = "/old/autter-3.1.0/autter checkpoint antigravity-pre --hook-input stdin";

    // Antigravity is installed and the user set up its hooks with an older
    // CLI whose binary has since moved.
    std::fs::create_dir_all(home.join(".gemini").join("antigravity")).unwrap();
    let hooks_path = home.join(".gemini").join("config").join("hooks.json");
    std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
    let stale_hooks = serde_json::json!({
        "my-linter": {"PostToolUse": [{"matcher": "run_command",
            "hooks": [{"type": "command", "command": "./lint.sh"}]}]},
        "autter": {
            "PreToolUse": [{"matcher": "write_to_file", "hooks": [{"type": "command",
                "command": stale_cmd, "timeout": 30}]}],
            "PostToolUse": [{"matcher": "write_to_file", "hooks": [{"type": "command",
                "command": "/old/autter-3.1.0/autter checkpoint antigravity-post --hook-input stdin",
                "timeout": 30}]}]
        }
    });
    std::fs::write(
        &hooks_path,
        serde_json::to_string_pretty(&stale_hooks).unwrap(),
    )
    .unwrap();
    // Claude Code is present but the user never set up autter hooks for it.
    std::fs::create_dir_all(home.join(".claude")).unwrap();

    // Hooks were last refreshed by an older version.
    let state_path = home
        .join(".autter")
        .join("internal")
        .join("hooks_version.json");
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    std::fs::write(&state_path, r#"{"version": "0.0.1"}"#).unwrap();

    // Before the refresh, doctor fails and names the agent and the command.
    let before = doctor_check(&repo, "Google Antigravity");
    assert_eq!(before["status"], "failed", "{before}");
    assert!(
        before["details"].to_string().contains(stale_cmd),
        "doctor must show the failing command: {before}"
    );
    assert!(
        before["remediation"]
            .as_str()
            .unwrap()
            .contains("autter install-hooks"),
        "{before}"
    );

    // Any command starts the refresh in the background (whoami exits 1 when
    // signed out; its own result doesn't matter here).
    run_in_test_home(&repo, &["whoami"], &[("AUTTER_TEST_HOOK_REFRESH", "1")]);

    let current = env!("CARGO_PKG_VERSION");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let state: Option<Value> = std::fs::read(&state_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        if state.as_ref().is_some_and(|s| s["version"] == current) {
            assert_eq!(state.unwrap()["failed"], serde_json::json!([]));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "hooks were not refreshed in time; state: {state:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    let refreshed: Value =
        serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();
    let pre = refreshed["autter"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(!pre.contains("/old/"), "hook still stale: {pre}");
    assert!(
        pre.ends_with("checkpoint antigravity-pre --hook-input stdin"),
        "{pre}"
    );
    assert_eq!(
        refreshed["my-linter"], stale_hooks["my-linter"],
        "the user's own hook is kept"
    );

    // Nothing installed for agents the user did not set up.
    for path in [
        home.join(".claude").join("settings.json"),
        home.join(".gemini").join("settings.json"),
        home.join(".cursor").join("hooks.json"),
        home.join(".codex").join("config.toml"),
    ] {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            !content.contains("autter"),
            "{} must not get autter hooks: {content}",
            path.display()
        );
    }

    // doctor now passes for the agent.
    let after = doctor_check(&repo, "Google Antigravity");
    assert_eq!(after["status"], "passed", "{after}");
}

const OLD_BINARY: &str = "/old/autter-3.1.0/autter";

/// Start the after-upgrade refresh and wait until it records the current
/// version. Clears the claim an earlier refresh in the same test left.
fn refresh_after_pretend_upgrade(repo: &TestRepo) {
    let internal = repo.test_home_path().join(".autter").join("internal");
    let state_path = internal.join("hooks_version.json");
    std::fs::create_dir_all(&internal).unwrap();
    std::fs::write(&state_path, r#"{"version": "0.0.1"}"#).unwrap();
    let _ = std::fs::remove_file(internal.join("hooks_refresh_claim.json"));

    run_in_test_home(repo, &["whoami"], &[("AUTTER_TEST_HOOK_REFRESH", "1")]);

    let current = env!("CARGO_PKG_VERSION");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let state: Option<Value> = std::fs::read(&state_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        if state.as_ref().is_some_and(|s| s["version"] == current) {
            assert_eq!(state.unwrap()["failed"], serde_json::json!([]));
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "hooks were not refreshed in time; state: {state:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Point every autter command in a hook config at `binary`, leaving the rest
/// of each command, and everything else in the file, as written.
fn repoint_hook_commands(value: &mut Value, binary: &str) -> usize {
    match value {
        Value::String(s) => {
            for sep in [" checkpoint ", " memory-hook "] {
                if let Some(at) = s.find(sep)
                    && s[..at].contains("autter")
                {
                    *s = format!("{binary}{}", &s[at..]);
                    return 1;
                }
            }
            0
        }
        Value::Array(items) => items
            .iter_mut()
            .map(|v| repoint_hook_commands(v, binary))
            .sum(),
        Value::Object(map) => map
            .values_mut()
            .map(|v| repoint_hook_commands(v, binary))
            .sum(),
        _ => 0,
    }
}

#[test]
fn moved_binary_is_out_of_date_for_agents_whose_layout_is_current() {
    // Claude Code, Droid, Gemini, Firebender and Windsurf: their hook layout
    // can be exactly what this CLI writes while every command still names a
    // binary that is gone (a nix store path, a dev build, a renamed home).
    // That used to count as up to date: the refresh skipped it and doctor
    // passed while nothing was captured.
    let repo = TestRepo::new();
    let home = repo.test_home_path().clone();
    let old = |preset: &str| format!("{OLD_BINARY} checkpoint {preset} --hook-input stdin");
    let command = |preset: &str| serde_json::json!({"type": "command", "command": old(preset)});

    // Hooks from an older CLI, in the layout each agent counts as installed.
    let windsurf_events: serde_json::Map<String, Value> = [
        "pre_write_code",
        "post_write_code",
        "pre_run_command",
        "post_run_command",
        "post_cascade_response_with_transcript",
    ]
    .iter()
    .map(|event| {
        (
            event.to_string(),
            serde_json::json!([{"command": old("windsurf")}]),
        )
    })
    .collect();
    let seeds: Vec<(&str, std::path::PathBuf, Value)> = vec![
        (
            "Claude Code",
            home.join(".claude").join("settings.json"),
            serde_json::json!({"hooks": {"PreToolUse": [
                {"matcher": "Write|Edit", "hooks": [command("claude")]}]}}),
        ),
        (
            "Droid",
            home.join(".factory").join("settings.json"),
            serde_json::json!({"hooks": {"PreToolUse": [
                {"matcher": "Edit", "hooks": [command("droid")]}]}}),
        ),
        (
            "Gemini",
            home.join(".gemini").join("settings.json"),
            serde_json::json!({"hooks": {"BeforeTool": [
                {"matcher": "write_file", "hooks": [command("gemini")]}]}}),
        ),
        (
            "Firebender",
            home.join(".firebender").join("hooks.json"),
            serde_json::json!({"hooks": {
                "preToolUse": [{"command": old("firebender")}],
                "postToolUse": [{"command": old("firebender")}]}}),
        ),
        (
            "Windsurf",
            home.join(".codeium").join("windsurf").join("hooks.json"),
            serde_json::json!({"hooks": windsurf_events}),
        ),
    ];
    for (_, path, seed) in &seeds {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(seed).unwrap()).unwrap();
    }

    // First refresh: every agent gets this CLI's layout and binary.
    refresh_after_pretend_upgrade(&repo);
    let mut configs: Vec<(&str, std::path::PathBuf)> = seeds
        .iter()
        .map(|(name, path, _)| (*name, path.clone()))
        .collect();
    // Windsurf has a second hooks file, written by the install.
    configs.push(("Windsurf", home.join(".codeium").join("hooks.json")));
    for (name, path) in &configs {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("{name}: {} not written: {e}", path.display()));
        assert!(
            !text.contains(OLD_BINARY),
            "{name}: first refresh left the old binary in {}:\n{text}",
            path.display()
        );
    }
    for (name, _, _) in &seeds {
        let check = doctor_check(&repo, name);
        assert_eq!(check["status"], "passed", "{name} after install: {check}");
    }

    // The binary moves: same layout, same commands, another path.
    for (name, path) in &configs {
        let mut config: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let repointed = repoint_hook_commands(&mut config, OLD_BINARY);
        assert!(
            repointed > 0,
            "{name}: no autter command in {}",
            path.display()
        );
        std::fs::write(path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    }

    // doctor fails and names the command that cannot run.
    for (name, _, _) in &seeds {
        let check = doctor_check(&repo, name);
        assert_eq!(
            check["status"], "failed",
            "{name} with a moved binary: {check}"
        );
        assert!(
            check["details"].to_string().contains(OLD_BINARY),
            "{name}: doctor must show the failing command: {check}"
        );
        assert!(
            check["remediation"]
                .as_str()
                .unwrap_or_default()
                .contains("autter install-hooks"),
            "{name}: {check}"
        );
    }

    // The next upgrade's refresh rewrites them.
    refresh_after_pretend_upgrade(&repo);
    for (name, path) in &configs {
        let text = std::fs::read_to_string(path).unwrap();
        assert!(
            !text.contains(OLD_BINARY),
            "{name}: refresh left the moved binary in {}:\n{text}",
            path.display()
        );
    }
    for (name, _, _) in &seeds {
        let check = doctor_check(&repo, name);
        assert_eq!(check["status"], "passed", "{name} after refresh: {check}");
    }
}

#[test]
fn unknown_subcommand_from_a_hook_fails_open() {
    // A hook written by another autter version can name a subcommand this one
    // does not have. Exit 2 would block the tool call in Claude Code.
    let repo = TestRepo::new();
    for envs in [&[][..], &[("AUTTER_TEST_DAEMON_UNAVAILABLE", "1")][..]] {
        for args in [
            vec!["capture", "claude", "--hook-input", "stdin"],
            vec!["capture", "antigravity-pre", "--hook-input=stdin"],
            vec!["--json", "capture", "claude", "--hook-input", "stdin"],
        ] {
            let run = run_hook(&repo, &args, r#"{"cwd": "/tmp"}"#, envs);
            let label = format!("{args:?} {envs:?}");
            assert_eq!(
                run.code,
                Some(0),
                "{label}: must exit 0\nstdout: {}\nstderr: {}",
                run.stdout,
                run.stderr
            );
            assert_eq!(run.stdout.trim(), "{}", "{label}: stderr: {}", run.stderr);
            if envs.is_empty() {
                assert!(
                    run.stderr.contains(STALE_MESSAGE) && run.stderr.contains("capture"),
                    "{label}: stderr must say the hook is stale and name the command: {}",
                    run.stderr
                );
            }
        }
    }

    // A person's typo is still a usage error, also with the hook variable
    // editor extensions set left in the environment.
    for envs in [&[][..], &[("AUTTER_CHECKPOINT_HOOK", "1")][..]] {
        let run = run_hook(&repo, &["capture", "claude"], "", envs);
        assert_eq!(run.code, Some(2), "{envs:?}: {}", run.stderr);
        assert!(run.stdout.trim().is_empty(), "{envs:?}: {}", run.stdout);
        assert!(
            run.stderr.contains("Unknown autter command: capture"),
            "{envs:?}: {}",
            run.stderr
        );
    }
}

#[test]
fn hook_commands_behind_a_global_flag_start_no_refresh() {
    // `autter --json uninstall-hooks` right after an upgrade must not start a
    // refresh that rewrites the files it is removing hooks from.
    let repo = TestRepo::new();
    let internal = repo.test_home_path().join(".autter").join("internal");
    std::fs::create_dir_all(&internal).unwrap();
    std::fs::write(
        internal.join("hooks_version.json"),
        r#"{"version": "0.0.1"}"#,
    )
    .unwrap();
    let claim = internal.join("hooks_refresh_claim.json");

    for args in [
        &["--json", "uninstall-hooks", "--help"][..],
        &["--json", "--version"],
        &["--quiet", "--json", "version"],
    ] {
        run_in_test_home(&repo, args, &[("AUTTER_TEST_HOOK_REFRESH", "1")]);
        assert!(!claim.exists(), "{args:?} started a hook refresh");
    }

    // The same flag in front of an ordinary command still does.
    run_in_test_home(
        &repo,
        &["--json", "whoami"],
        &[("AUTTER_TEST_HOOK_REFRESH", "1")],
    );
    assert!(claim.exists(), "`--json whoami` must start the refresh");
}
