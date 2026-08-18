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
