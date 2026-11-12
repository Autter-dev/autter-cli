//! Integration tests for `autter doctor`.
//!
//! The test harness wires trace2 into the per-test daemon via the
//! `GIT_TRACE2_EVENT` environment variable, while doctor validates the
//! production wiring (global git config). So the end-to-end test first writes
//! the trace2 keys `autter install` would write -- pointed at this test's
//! daemon socket -- and then expects the full checkpoint round-trip to pass.

use crate::repos::test_repo::TestRepo;
use autter::daemon::DaemonConfig;

/// Write the trace2 global config `autter install` sets up in production,
/// targeting this test repo's isolated daemon socket.
fn write_trace2_global_config(repo: &TestRepo) {
    let target = DaemonConfig::trace2_event_target_for_path(&repo.daemon_trace_socket_path());
    repo.git_og(&["config", "--global", "trace2.eventTarget", &target])
        .expect("write trace2.eventTarget");
    repo.git_og(&["config", "--global", "trace2.eventNesting", "10"])
        .expect("write trace2.eventNesting");
}

/// Doctor prints the JSON report as a single stdout line; the combined
/// stdout/stderr capture may carry other noise around it.
fn parse_doctor_json(raw: &str) -> serde_json::Value {
    raw.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('{') {
                serde_json::from_str(trimmed).ok()
            } else {
                None
            }
        })
        .next_back()
        .unwrap_or_else(|| panic!("no JSON object line in doctor output:\n{}", raw))
}

fn check<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("missing check '{}' in report:\n{}", name, report))
}

#[test]
fn doctor_validates_checkpoint_round_trip_end_to_end() {
    let repo = TestRepo::new();
    write_trace2_global_config(&repo);

    let (raw, exited_zero) = match repo.autter(&["doctor", "--json"]) {
        Ok(output) => (output, true),
        Err(output) => (output, false),
    };
    let report = parse_doctor_json(&raw);

    // Every expected check is present.
    for name in [
        "git version",
        "configuration file",
        "repository",
        "background service",
        "trace2 ingestion",
        "trace2 config (configured git)",
        "trace2 config (terminal git)",
        "trace2 event capture",
        "checkpoint round-trip",
        "authentication",
        "cloud connectivity",
    ] {
        check(&report, name);
    }

    // The environment-independent core: setup, service, capture wiring, and a
    // real checkpoint event flowing checkpoint -> daemon -> commit -> blame.
    assert_eq!(check(&report, "git version")["status"], "passed", "{}", raw);
    assert_eq!(
        check(&report, "configuration file")["status"],
        "passed",
        "{}",
        raw
    );
    assert_eq!(check(&report, "repository")["status"], "passed", "{}", raw);
    assert_eq!(
        check(&report, "background service")["status"],
        "passed",
        "{}",
        raw
    );
    assert_eq!(
        check(&report, "trace2 config (configured git)")["status"],
        "passed",
        "{}",
        raw
    );
    assert_eq!(
        check(&report, "checkpoint round-trip")["status"],
        "passed",
        "{}",
        raw
    );

    // Tests run logged out: auth must not hard-fail, and the connectivity
    // probe must be skipped so tests never touch the network.
    assert_ne!(
        check(&report, "authentication")["status"],
        "failed",
        "{}",
        raw
    );
    assert_eq!(
        check(&report, "cloud connectivity")["status"],
        "skipped",
        "{}",
        raw
    );

    // Summary bookkeeping and exit code track the failed count. (Agent-hook
    // checks may legitimately fail on developer machines with agents
    // installed outside the isolated test HOME, so overall success is not
    // asserted -- only consistency.)
    let checks = report["checks"].as_array().unwrap();
    let failed = checks.iter().filter(|c| c["status"] == "failed").count() as u64;
    assert_eq!(
        report["summary"]["failed"].as_u64().unwrap(),
        failed,
        "{}",
        raw
    );
    assert_eq!(report["ok"].as_bool().unwrap(), failed == 0, "{}", raw);
    assert_eq!(exited_zero, failed == 0, "{}", raw);
}

/// `autter doctor` and `autter bg status` share one health probe, so they
/// must reach the same verdict about the background service.
#[test]
fn doctor_and_bg_status_agree_on_service_health() {
    let repo = TestRepo::new();
    write_trace2_global_config(&repo);

    let raw_status = repo
        .autter(&["bg", "status"])
        .expect("bg status should succeed for a running service");
    let start = raw_status.find('{').expect("bg status prints JSON");
    let end = raw_status.rfind('}').expect("bg status prints JSON");
    let status: serde_json::Value =
        serde_json::from_str(&raw_status[start..=end]).expect("valid bg status JSON");
    assert_eq!(status["health"]["state"], "healthy", "{raw_status}");
    assert_eq!(status["ok"], true, "{raw_status}");
    assert_eq!(
        status["git_capture"]["trace2_configured"], true,
        "{raw_status}"
    );
    assert!(
        !raw_status.contains("git capture (trace2) is not configured"),
        "{raw_status}"
    );

    let raw = repo
        .autter(&["doctor", "--json"])
        .unwrap_or_else(|output| output);
    let report = parse_doctor_json(&raw);
    let service = check(&report, "background service");
    assert_eq!(service["status"], "passed", "{raw}");
    assert_eq!(service["summary"], "background service is running", "{raw}");
}

#[test]
fn doctor_rejects_unknown_arguments() {
    let repo = TestRepo::new();
    let err = repo
        .autter(&["doctor", "--definitely-not-a-flag"])
        .expect_err("unknown doctor argument should fail");
    assert!(
        err.contains("unknown doctor argument"),
        "unexpected error output: {}",
        err
    );
}

#[test]
fn doctor_skip_trace2_flag_skips_event_capture_check() {
    let repo = TestRepo::new();
    write_trace2_global_config(&repo);

    let raw = match repo.autter(&["doctor", "--json", "--skip-trace2-checks"]) {
        Ok(output) => output,
        Err(output) => output,
    };
    let report = parse_doctor_json(&raw);
    assert_eq!(
        check(&report, "trace2 event capture")["status"],
        "skipped",
        "{}",
        raw
    );
}

/// `bg status` used to say "healthy" while doctor failed trace2: a running
/// service records nothing unless git's trace2 config points at it.
#[test]
fn bg_status_reports_missing_git_capture() {
    let repo = TestRepo::new();
    let raw_status = repo
        .autter(&["bg", "status"])
        .expect("bg status still succeeds: the service itself is healthy");
    let start = raw_status.find('{').expect("bg status prints JSON");
    let end = raw_status.rfind('}').expect("bg status prints JSON");
    let status: serde_json::Value =
        serde_json::from_str(&raw_status[start..=end]).expect("valid bg status JSON");
    assert_eq!(
        status["git_capture"]["trace2_configured"], false,
        "{raw_status}"
    );
    assert!(
        raw_status.contains("git capture (trace2) is not configured"),
        "{raw_status}"
    );
    assert!(
        raw_status.contains("autter install --system"),
        "{raw_status}"
    );
}

/// A trace2 config autter wrote for an old service location is repaired by
/// doctor instead of sending the user to re-run setup.
#[test]
fn doctor_repairs_a_stale_autter_trace2_target() {
    let repo = TestRepo::new();
    repo.git_og(&[
        "config",
        "--global",
        "trace2.eventTarget",
        "af_unix:stream:/nonexistent/old-home/.autter/internal/daemon/trace2.sock",
    ])
    .expect("write a stale trace2.eventTarget");
    repo.git_og(&["config", "--global", "trace2.eventNesting", "10"])
        .expect("write trace2.eventNesting");

    let _ = repo.autter(&["doctor", "--json"]);

    let expected = DaemonConfig::trace2_event_target_for_path(&repo.daemon_trace_socket_path());
    let actual = repo
        .git_og(&["config", "--global", "--get-all", "trace2.eventTarget"])
        .expect("trace2.eventTarget is still set");
    assert_eq!(
        actual.trim(),
        expected,
        "doctor should repoint the stale target"
    );
}

/// Doctor never writes trace2 for someone who has none, or replaces a target
/// that isn't autter's: that needs explicit consent (`install --system`).
#[test]
fn doctor_leaves_foreign_trace2_targets_alone() {
    let repo = TestRepo::new();
    let foreign = repo.test_home_path().join("my-own-trace.json");
    let foreign = foreign.to_string_lossy().to_string();
    repo.git_og(&["config", "--global", "trace2.eventTarget", &foreign])
        .expect("write a foreign trace2.eventTarget");

    let _ = repo.autter(&["doctor", "--json"]);

    let actual = repo
        .git_og(&["config", "--global", "--get-all", "trace2.eventTarget"])
        .expect("trace2.eventTarget is still set");
    assert_eq!(actual.trim(), foreign);
}

/// `autter doctor --report <file>` writes one support file with the results,
/// the service and lock-file state, and the end of the service log, and
/// still prints the normal report.
#[test]
fn doctor_report_writes_a_support_file() {
    let repo = TestRepo::new();
    write_trace2_global_config(&repo);
    let report_path = repo.test_home_path().join("autter-report.txt");
    let report_arg = report_path.to_string_lossy().to_string();

    let raw = repo
        .autter(&["doctor", "--json", "--report", &report_arg])
        .unwrap_or_else(|output| output);
    let report = parse_doctor_json(&raw);
    assert_eq!(
        check(&report, "background service")["status"],
        "passed",
        "{raw}"
    );
    assert!(raw.contains("Report written to"), "{raw}");

    let text = std::fs::read_to_string(&report_path).expect("report file written");
    for section in [
        "autter doctor report",
        "== system",
        "elevated (root / Administrator): ",
        "configured git: ",
        "== doctor results",
        "background service: background service is running",
        "== background service",
        "control socket: ",
        "accepting, answers requests",
        "timings measured by this doctor run:",
        "service health probe: Healthy",
        "== lock file",
        "state: held",
        "== service log",
    ] {
        assert!(text.contains(section), "missing {section:?} in:\n{text}");
    }

    let err = repo
        .autter(&["doctor", "--report"])
        .expect_err("--report without a path is a usage error");
    assert!(err.contains("--report needs a file path"), "{err}");
}

/// `autter doctor --agent <name>` sends the agent's own pre- and post-edit
/// hook payloads through `autter checkpoint <preset> --hook-input stdin` in a
/// throwaway repository and passes only when the background service recorded
/// an AI checkpoint for the edited file. Every agent with hooks is covered,
/// so verifying one no longer needs a real agent session.
#[test]
fn doctor_agent_simulates_each_agents_hook_calls() {
    let repo = TestRepo::new();
    for agent in [
        "claude",
        "codex",
        "cursor",
        "copilot",
        "amp",
        "opencode",
        "pi",
        "gemini",
        "droid",
        "firebender",
        "antigravity",
        "windsurf",
    ] {
        let raw = repo
            .autter(&["doctor", "--agent", agent, "--json"])
            .unwrap_or_else(|output| output);
        let report = parse_doctor_json(&raw);
        for name in [
            "detected",
            "hook files",
            "hook commands",
            "background service",
        ] {
            check(&report, name);
        }
        assert_eq!(
            check(&report, "pre-edit hook")["status"],
            "passed",
            "{agent}: {raw}"
        );
        let post = check(&report, "post-edit hook");
        assert_eq!(post["status"], "passed", "{agent}: {raw}");
        assert!(
            post["summary"]
                .as_str()
                .unwrap()
                .contains("recorded an AI checkpoint"),
            "{agent}: {raw}"
        );
    }
    // A simulated Copilot call is not Copilot capture working: doctor's
    // Copilot heartbeat must stay untouched.
    let heartbeat = repo
        .test_home_path()
        .join(".autter/internal/hook_heartbeats/github-copilot.json");
    assert!(
        !heartbeat.exists(),
        "simulated hook calls wrote a heartbeat"
    );
}

#[test]
fn doctor_agent_for_an_editor_extension_skips_the_simulation() {
    let repo = TestRepo::new();
    let raw = repo
        .autter(&["doctor", "--agent", "jetbrains", "--json"])
        .unwrap_or_else(|output| output);
    let report = parse_doctor_json(&raw);
    let sim = check(&report, "simulated edit");
    assert_eq!(sim["status"], "skipped", "{raw}");
    assert!(
        sim["summary"].as_str().unwrap().contains("extension"),
        "{raw}"
    );
}

#[test]
fn doctor_agent_names_the_agents_it_knows() {
    let repo = TestRepo::new();
    let err = repo
        .autter(&["doctor", "--agent", "notepad"])
        .expect_err("an unknown agent is a usage error");
    assert!(err.contains("unknown agent 'notepad'"), "{err}");
    assert!(err.contains("claude-code"), "{err}");
    assert!(err.contains("docs/agents.md"), "{err}");

    let err = repo
        .autter(&["doctor", "--agent"])
        .expect_err("--agent needs a name");
    assert!(err.contains("--agent needs an agent name"), "{err}");
}
