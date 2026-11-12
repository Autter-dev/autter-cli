//! The usage-metrics upload queue as a user sees it: `autter sync status`
//! counts the events that were set aside, `autter sync retry` queues them
//! again, and only `autter sync purge` resets what the queue learned.

use crate::repos::test_repo::TestRepo;
use serde_json::Value;

fn internal_dir(repo: &TestRepo) -> std::path::PathBuf {
    repo.test_home_path().join(".autter").join("internal")
}

fn sync_status(repo: &TestRepo) -> Value {
    let output = repo
        .autter_command(&["sync", "status", "--json"])
        .output()
        .expect("run autter sync status");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "sync status --json: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn sync_status_counts_set_aside_metrics_and_sync_retry_queues_them_again() {
    let repo = TestRepo::new();
    // The first run creates the queue database.
    let before = sync_status(&repo);
    assert_eq!(before["cloud_sync"]["usage_metrics"]["set_aside"], 0);
    assert!(
        before["cloud_sync"]["usage_metrics"]
            .get("set_aside_reason")
            .is_none()
    );

    // What the background service does with an event the server fails on
    // every time: it moves it to the set-aside table, with the error.
    let db = rusqlite::Connection::open(internal_dir(&repo).join("metrics-db")).unwrap();
    db.execute(
        "INSERT INTO metrics_set_aside (event_json, reason, set_aside_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![
            r#"{"t":1760000000,"e":1,"v":{},"a":{}}"#,
            "HTTP 500: Data-plane operation failed",
            1_762_588_800_i64
        ],
    )
    .unwrap();
    drop(db);

    let status = sync_status(&repo);
    let metrics = &status["cloud_sync"]["usage_metrics"];
    assert_eq!(metrics["set_aside"], 1, "{metrics}");
    assert_eq!(
        metrics["set_aside_reason"],
        "HTTP 500: Data-plane operation failed"
    );
    assert_eq!(
        metrics["queued"], 0,
        "set-aside events are not in the queue"
    );

    let retried = repo.autter(&["sync", "retry"]).unwrap();
    assert!(retried.contains("Queued 1 set-aside"), "{retried}");
    let status = sync_status(&repo);
    let metrics = &status["cloud_sync"]["usage_metrics"];
    assert_eq!(metrics["set_aside"], 0, "{metrics}");
    assert_eq!(metrics["queued"], 1, "the event is back in the queue");

    let nothing = repo.autter(&["sync", "retry"]).unwrap();
    assert!(nothing.contains("Nothing is set aside"), "{nothing}");
}

#[test]
fn sync_purge_deletes_set_aside_metrics_and_resets_the_metrics_state() {
    let repo = TestRepo::new();
    sync_status(&repo);
    let internal = internal_dir(&repo);
    let state = internal.join("metrics-sync-state.json");
    std::fs::write(
        &state,
        r#"{"batch_limit":125,"consecutive_failures":3,"next_attempt_at":99999999999}"#,
    )
    .unwrap();
    let db = rusqlite::Connection::open(internal.join("metrics-db")).unwrap();
    db.execute(
        "INSERT INTO metrics_set_aside (event_json, reason, set_aside_at) VALUES ('{}', 'HTTP 500', 0)",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        sync_status(&repo)["cloud_sync"]["usage_metrics"]["batch_size"],
        125
    );

    let purged = repo.autter(&["sync", "purge", "--force"]).unwrap();
    assert!(purged.contains("1 telemetry events"), "{purged}");
    assert!(!state.exists(), "purge resets the metrics state");
    let metrics = sync_status(&repo)["cloud_sync"]["usage_metrics"].clone();
    assert_eq!(metrics["set_aside"], 0, "{metrics}");
    assert_eq!(metrics["batch_size"], 1000, "{metrics}");
}
