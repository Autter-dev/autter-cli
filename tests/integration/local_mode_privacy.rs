//! Local mode must never upload to the Autter platform, and must not pile up
//! platform-only data in upload queues.
//!
//! `autter onboard --local` writes `prompt_storage = "local"` and promises
//! "Nothing is uploaded to the Autter platform." These tests run a dedicated
//! daemon (which reads the test HOME config) in local vs connected mode and
//! compare the durable upload queues reported by `autter sync status --json`.
//! The tests are not logged in, so in connected mode queued data stays queued,
//! which makes the connected run a control for the local-mode assertions.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use autter::config::{NotesBackendConfig, NotesBackendKind};
use autter::daemon::{ControlRequest, TelemetryEnvelope, send_control_request};
use autter::metrics::{CommittedValues, EventAttributes, MetricEvent, PosEncoded};
use serde_json::Value;
use std::fs;
use std::time::{Duration, Instant};

fn local_mode_repo() -> TestRepo {
    TestRepo::new_dedicated_daemon_with_initial_config(|patch| {
        patch.prompt_storage = Some("local".to_string());
        patch.telemetry_oss_disabled = Some(true);
        patch.feature_flags = Some(serde_json::json!({"transcript_sweep": false}));
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::GitNotes,
            backend_url: None,
        });
    })
}

fn connected_mode_repo() -> TestRepo {
    TestRepo::new_dedicated_daemon_with_initial_config(|patch| {
        patch.prompt_storage = Some("default".to_string());
        patch.telemetry_oss_disabled = Some(true);
        patch.feature_flags = Some(serde_json::json!({"transcript_sweep": false}));
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::GitNotes,
            backend_url: None,
        });
    })
}

/// `cloud_sync.pending` from `autter sync status --json`.
fn pending_counts(repo: &TestRepo) -> Value {
    let output = repo
        .autter(&["sync", "status", "--json"])
        .unwrap_or_else(|e| e);
    let start = output
        .find('{')
        .unwrap_or_else(|| panic!("sync status should print JSON, got: {output}"));
    let json: Value = serde_json::from_str(output[start..].trim())
        .unwrap_or_else(|e| panic!("invalid sync status JSON ({e}): {output}"));
    json["cloud_sync"]["pending"].clone()
}

fn checkpoint_and_commit(repo: &TestRepo) {
    let file_path = repo.path().join("hot.rs");
    fs::write(&file_path, "line1\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    fs::write(&file_path, "line1\nline2\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "hot.rs"])
        .unwrap();
    fs::write(&file_path, "line1\nline2\nline3\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "hot.rs"])
        .unwrap();

    let mut ai_file = repo.filename("ai.rs");
    ai_file.set_contents(lines!["fn ai() {}".ai()]);
    repo.stage_all_and_commit("second").unwrap();
    repo.sync_daemon();
}

fn metric_event() -> MetricEvent {
    let values = CommittedValues::new()
        .human_additions(1)
        .git_diff_added_lines(1)
        .git_diff_deleted_lines(0)
        .tool_model_pairs(vec!["all".to_string()])
        .ai_additions(vec![0]);
    let attrs = EventAttributes::with_version("0.0.0-test")
        .tool("integration-test")
        .commit_sha("0123456789abcdef0123456789abcdef01234567");
    MetricEvent::new(&values, attrs.to_sparse())
}

/// Hand metric events straight to the daemon's telemetry worker (the path
/// every `metrics::record` call takes) and wait for its flush ticks.
fn submit_metrics_and_wait(repo: &TestRepo) {
    let request = ControlRequest::SubmitTelemetry {
        envelopes: vec![TelemetryEnvelope::Metrics {
            events: vec![metric_event(), metric_event()],
        }],
    };
    let response = send_control_request(&repo.daemon_control_socket_path(), &request)
        .expect("daemon should accept telemetry");
    assert!(response.ok, "telemetry submit failed: {:?}", response.error);
}

fn wait_for_pending_metrics(repo: &TestRepo, at_least: i64, timeout: Duration) -> i64 {
    let deadline = Instant::now() + timeout;
    loop {
        let metrics = pending_counts(repo)["metrics"].as_i64().unwrap_or(0);
        if metrics >= at_least || Instant::now() >= deadline {
            return metrics;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn local_mode_keeps_file_change_counts_but_queues_nothing_for_upload() {
    let repo = local_mode_repo();
    checkpoint_and_commit(&repo);

    // The local feature still works.
    let output = repo.autter(&["file-changes", "--json"]).unwrap();
    let json: Value = serde_json::from_str(output.trim()).unwrap();
    let files = json["files"].as_array().expect("files array");
    assert!(
        files.iter().any(|f| f["path"] == "hot.rs"),
        "local file-change counts must still be recorded: {json}"
    );

    // Nothing is waiting for upload.
    let pending = pending_counts(&repo);
    assert_eq!(pending["file_changes"], 0, "pending: {pending}");
    assert_eq!(pending["commit_summaries"], 0, "pending: {pending}");
    assert_eq!(pending["transcripts"], 0, "pending: {pending}");
    assert_eq!(pending["notes"], 0, "pending: {pending}");
    assert_eq!(pending["total"], 0, "pending: {pending}");
}

#[test]
fn connected_mode_queues_file_changes_for_upload() {
    // Control for the local-mode test: the same flow in connected mode (not
    // logged in) leaves rows queued for upload.
    let repo = connected_mode_repo();
    checkpoint_and_commit(&repo);

    let pending = pending_counts(&repo);
    assert!(
        pending["file_changes"].as_i64().unwrap_or(0) > 0,
        "connected mode should queue file changes; pending: {pending}"
    );
}

#[test]
fn local_mode_daemon_drops_metrics_instead_of_queueing_them() {
    let repo = local_mode_repo();
    submit_metrics_and_wait(&repo);
    // Several 3-second flush ticks.
    std::thread::sleep(Duration::from_secs(7));
    let pending = pending_counts(&repo);
    assert_eq!(
        pending["metrics"], 0,
        "local mode must not queue metrics for upload; pending: {pending}"
    );
}

#[test]
fn connected_mode_daemon_queues_metrics_when_not_logged_in() {
    // Control for the local-mode metrics test: proves the submitted events do
    // reach the flush path that would otherwise store them for upload.
    let repo = connected_mode_repo();
    submit_metrics_and_wait(&repo);
    let metrics = wait_for_pending_metrics(&repo, 2, Duration::from_secs(15));
    assert!(
        metrics >= 2,
        "connected mode should queue metrics for retry while logged out; got {metrics}"
    );
}

#[test]
fn doctor_reports_local_mode_as_not_uploading() {
    let repo = local_mode_repo();
    let output = repo.autter(&["doctor", "--json"]).unwrap_or_else(|e| e);
    assert!(
        output.contains("local mode — nothing is uploaded to the Autter platform"),
        "doctor should describe local mode as not uploading: {output}"
    );
}

/// Exercise the real daemon reader and its outgoing metrics queue. The marker
/// is ordinary prompt text, so secret redaction cannot hide a missing repo gate.
fn streamed_prompt_is_queued(excluded: bool) -> bool {
    let repo = TestRepo::new_dedicated_daemon_with_initial_config(|patch| {
        patch.prompt_storage = Some("default".to_string());
        patch.telemetry_oss_disabled = Some(true);
        patch.disable_version_checks = Some(true);
        patch.disable_auto_updates = Some(true);
        patch.feature_flags = Some(serde_json::json!({
            "transcript_streaming": true, "transcript_sweep": false
        }));
        patch.exclude_prompts_in_repositories = Some(if excluded {
            vec!["https://github.com/privacy-test/*".to_string()]
        } else {
            vec![]
        });
    });
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/privacy-test/private.git",
    ])
    .unwrap();
    let cwd = repo.canonical_path();
    fs::write(cwd.join("source.rs"), "fn original() {}\n").unwrap();
    repo.stage_all_and_commit("Initial stream fixture").unwrap();
    repo.filename("source.rs")
        .assert_committed_lines(lines!["fn original() {}".unattributed_human(),]);
    let transcript = repo.test_home_path().join("privacy-transcript.jsonl");
    let marker = "Private meeting agenda for the garden project";
    let session = "privacy-session";
    let metadata = serde_json::json!({
        "type": "session_meta", "payload": { "id": session, "cwd": cwd }
    });
    let message = serde_json::json!({
        "type": "response_item", "payload": {
            "type": "message", "role": "user",
            "content": [{ "type": "input_text", "text": marker }]
        }
    });
    fs::write(&transcript, format!("{metadata}\n{message}\n")).unwrap();
    fs::write(cwd.join("source.rs"), "fn example() {}\n").unwrap();
    let hook = serde_json::json!({
        "session_id": session, "cwd": cwd, "hook_event_name": "PostToolUse",
        "tool_name": "apply_patch", "tool_use_id": "privacy-edit",
        "tool_input": {"patch": format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-fn original() {{}}\n+fn example() {{}}\n*** End Patch",
            cwd.join("source.rs").display()
        )},
        "tool_response": "A source.rs", "transcript_path": transcript
    })
    .to_string();
    repo.autter(&["checkpoint", "codex", "--hook-input", &hook])
        .unwrap();

    let streams_path = repo
        .daemon_home_path()
        .join(".autter/internal/transcripts-db");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let processed = rusqlite::Connection::open(&streams_path)
            .ok()
            .and_then(|db| {
                db.query_row(
                    "SELECT MAX(CAST(watermark_value AS INTEGER)) FROM tracked_streams",
                    [],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .ok()
                .flatten()
            })
            .unwrap_or(0);
        if processed > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "transcript worker did not finish the fixture"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // The worker has consumed the complete fixture; allow two flush ticks.
    std::thread::sleep(Duration::from_secs(7));
    let metrics_path = repo.daemon_home_path().join(".autter/internal/metrics-db");
    if !metrics_path.exists() {
        return false;
    }
    let metrics = rusqlite::Connection::open(metrics_path).unwrap();
    let count: i64 = metrics
        .query_row(
            "SELECT COUNT(*) FROM metrics WHERE instr(event_json, ?1) > 0",
            [marker],
            |row| row.get(0),
        )
        .unwrap();
    count > 0
}

#[test]
fn transcript_stream_respects_repository_prompt_exclusion() {
    assert!(
        streamed_prompt_is_queued(false),
        "control: allowed prompt should reach the retry queue"
    );
    assert!(
        !streamed_prompt_is_queued(true),
        "excluded repository prompt reached the outgoing retry queue"
    );
}
