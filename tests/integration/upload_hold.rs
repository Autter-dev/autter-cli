//! Upload backlogs that predate the user's consent must be held, not drained.
//!
//! - Data queued by autter 2.2.0 and earlier (which queued transcripts with
//!   local prompt storage and for excluded repos, and metrics/file changes in
//!   local mode) is held by a one-time migration until `autter sync import`.
//! - `autter onboard --connect` holds the existing backlog before connecting;
//!   a non-interactive run without `--yes` keeps it held and says how to
//!   import or delete it.
//!
//! These run without a background service, so nothing can drain the queues
//! behind the test's back; "held" is observed through `autter sync status`.

use crate::repos::test_repo::{DaemonTestScope, GitTestMode, TestRepo};
use autter::file_changes::FileChangesDatabase;
use autter::notes::db::NotesDatabase;
use serde_json::Value;
use std::fs;

fn repo_without_service() -> TestRepo {
    TestRepo::new_with_mode_and_daemon_scope(GitTestMode::Daemon, DaemonTestScope::NoDaemon)
}

/// Queue upload items directly, the way an older autter version left them.
fn seed_backlog(repo: &TestRepo) {
    let file_changes_path = format!("{}-file-changes", repo.test_db_path().display());
    let mut file_changes =
        FileChangesDatabase::open_at_path(std::path::Path::new(&file_changes_path)).unwrap();
    file_changes
        .record_change("https://github.com/acme/repo", "old.rs", 3, 1, 1_000, true)
        .unwrap();

    let notes_path = repo
        .test_home_path()
        .join(".autter")
        .join("internal")
        .join("notes-db");
    let mut notes = NotesDatabase::open_at_path(&notes_path).unwrap();
    notes.upsert_note("0123abcd", "{}").unwrap();
    notes
        .enqueue_commit_summary("0123abcd", r#"{"commit_sha":"0123abcd"}"#)
        .unwrap();
}

fn sync_status(repo: &TestRepo) -> Value {
    let output = repo
        .autter(&["sync", "status", "--json"])
        .unwrap_or_else(|e| e);
    let start = output.find('{').expect("sync status prints JSON");
    let json: Value = serde_json::from_str(output[start..].trim()).unwrap();
    json["cloud_sync"].clone()
}

fn hold_marker(repo: &TestRepo) -> std::path::PathBuf {
    repo.test_home_path()
        .join(".autter")
        .join("internal")
        .join("upload_hold.json")
}

/// Write credentials that load as a valid login without any network call.
fn sign_in_offline(repo: &TestRepo) {
    let dir = repo.test_home_path().join(".autter").join("internal");
    fs::create_dir_all(&dir).unwrap();
    let far_future = 4_102_444_800i64; // 2100-01-01
    fs::write(
        dir.join("credentials"),
        serde_json::json!({
            "access_token": "test-access-token",
            "refresh_token": "test-refresh-token",
            "access_token_expires_at": far_future,
            "refresh_token_expires_at": far_future,
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn legacy_backlog_is_held_until_explicit_import() {
    let repo = repo_without_service();
    let _ = fs::remove_file(hold_marker(&repo));
    seed_backlog(&repo);

    // The first command that can drain or inspect the hold applies the
    // one-time migration. Without --yes, import only reports what is held.
    let output = repo.autter(&["sync", "import"]).unwrap();
    assert!(output.contains("Held for your consent"), "{output}");
    assert!(
        hold_marker(&repo).exists(),
        "migration marker must be written"
    );

    let status = sync_status(&repo);
    assert_eq!(status["pending"]["file_changes"], 0, "{status}");
    assert_eq!(status["pending"]["notes"], 0, "{status}");
    assert_eq!(status["pending"]["commit_summaries"], 0, "{status}");
    assert_eq!(status["held"]["file_changes"], 1, "{status}");
    assert_eq!(status["held"]["notes"], 1, "{status}");
    assert_eq!(status["held"]["commit_summaries"], 1, "{status}");

    // The migration runs once: data queued afterwards is not held.
    let file_changes_path = format!("{}-file-changes", repo.test_db_path().display());
    FileChangesDatabase::open_at_path(std::path::Path::new(&file_changes_path))
        .unwrap()
        .record_change("https://github.com/acme/repo", "new.rs", 1, 0, 2_000, true)
        .unwrap();
    let status = sync_status(&repo);
    assert_eq!(status["pending"]["file_changes"], 1, "{status}");
    assert_eq!(status["held"]["file_changes"], 1, "{status}");

    // Explicit consent releases the held backlog.
    let output = repo.autter(&["sync", "import", "--yes"]).unwrap();
    assert!(output.contains("Released for upload"), "{output}");
    let status = sync_status(&repo);
    assert_eq!(status["held"]["file_changes"], 0, "{status}");
    assert_eq!(status["pending"]["file_changes"], 2, "{status}");
    assert_eq!(status["pending"]["notes"], 1, "{status}");
}

#[test]
fn sync_import_refuses_in_local_mode() {
    let mut repo = repo_without_service();
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("local".to_string()));
    let _ = fs::remove_file(hold_marker(&repo));
    seed_backlog(&repo);

    let result = repo.autter(&["sync", "import", "--yes"]);
    let output = result.clone().unwrap_or_else(|e| e);
    assert!(result.is_err(), "import must fail in local mode: {output}");
    assert!(output.contains("local mode"), "{output}");
    assert!(sync_status(&repo)["held"]["file_changes"].as_i64().unwrap() > 0);
}

#[test]
fn non_interactive_connect_without_yes_keeps_backlog_held() {
    let mut repo = repo_without_service();
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("local".to_string()));
    // Already migrated, so only the connect-time hold is under test.
    let _ = repo.autter(&["sync", "status", "--json"]);
    let _ = repo.autter(&["sync", "import"]);
    seed_backlog(&repo);
    assert!(
        sync_status(&repo)["pending"]["file_changes"]
            .as_i64()
            .unwrap()
            > 0
    );

    sign_in_offline(&repo);
    // The config patch env would pin prompt_storage to "local"; drop it so
    // onboarding's saved choice (connected) is what counts afterwards.
    repo.patch_autter_config(|patch| patch.prompt_storage = None);
    let output = repo
        .autter(&["onboard", "--connect", "--no-telemetry"])
        .unwrap_or_else(|e| e);
    assert!(
        output.contains("NOT uploaded"),
        "non-interactive connect must keep the backlog: {output}"
    );
    assert!(output.contains("autter sync import --yes"), "{output}");
    assert!(output.contains("autter sync purge --force"), "{output}");

    let status = sync_status(&repo);
    assert_eq!(status["pending"]["file_changes"], 0, "{status}");
    assert_eq!(status["pending"]["notes"], 0, "{status}");
    assert_eq!(status["held"]["file_changes"], 1, "{status}");
    assert_eq!(status["held"]["notes"], 1, "{status}");
}

#[test]
fn non_interactive_connect_with_yes_imports_backlog() {
    let mut repo = repo_without_service();
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("local".to_string()));
    let _ = repo.autter(&["sync", "import"]);
    seed_backlog(&repo);

    sign_in_offline(&repo);
    repo.patch_autter_config(|patch| patch.prompt_storage = None);
    let output = repo
        .autter(&["onboard", "--connect", "--yes", "--no-telemetry"])
        .unwrap_or_else(|e| e);
    assert!(output.contains("Backlog released"), "{output}");

    let status = sync_status(&repo);
    assert_eq!(status["held"]["file_changes"], 0, "{status}");
    assert_eq!(status["pending"]["file_changes"], 1, "{status}");
}
