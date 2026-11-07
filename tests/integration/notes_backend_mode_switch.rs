//! Regression tests: switching the notes backend mode (what `autter onboard`
//! does when the user picks connected vs local-only) must take effect in the
//! long-lived daemon without a restart.
//!
//! The daemon previously dispatched note writes on the process-lifetime
//! `Config::get()` snapshot taken at startup. A user who onboarded in
//! connected mode (http backend) and later switched to local-only (git_notes)
//! kept a daemon that routed every authorship note to the HTTP queue —
//! refs/notes/ai was never written, so `autter blame` degraded to plain git
//! blame in exactly the mode whose only note source is git refs.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use autter::config::{NotesBackendConfig, NotesBackendKind};
use autter::notes::db::NotesDatabase;

#[test]
fn switch_to_local_only_writes_git_notes_without_daemon_restart() {
    // The daemon starts while the machine is in connected mode (http backend).
    let mut repo = TestRepo::new_dedicated_daemon_with_initial_config(|patch| {
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: None,
        });
    });

    // A first commit under connected mode forces the daemon's note-write path
    // to run (and with it any process-global config initialization) while the
    // config still says http. Committed with raw git because the harness
    // commit helpers fetch the authorship log from git notes, which the http
    // backend intentionally does not write.
    let mut first = repo.filename("first.txt");
    first.set_contents(lines!["connected mode line".ai()]);
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "commit while connected"])
        .unwrap();
    repo.sync_daemon();

    // Sanity-check the daemon really is running with the http backend: the
    // note for this commit must have gone to the notes queue, not git refs.
    let notes_after_first = repo.git(&["notes", "--ref=ai", "list"]).unwrap_or_default();
    assert!(
        notes_after_first.trim().is_empty(),
        "http backend must not write refs/notes/ai; the initial connected-mode \
         config did not reach the daemon (notes list: {notes_after_first:?})"
    );

    // Switch to local-only mode — the same config change `autter onboard`
    // makes for "Local only" — without restarting the daemon.
    repo.patch_autter_config(|patch| {
        patch.prompt_storage = Some("local".to_string());
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::GitNotes,
            backend_url: None,
        });
    });

    let mut second = repo.filename("second.txt");
    second.set_contents(lines!["local mode line".ai()]);
    let commit = repo
        .stage_all_and_commit("commit while local-only")
        .unwrap();
    repo.sync_daemon();

    // Local-only mode reads attribution exclusively from refs/notes/ai, so
    // the note for the post-switch commit must exist there.
    let notes_list = repo.git(&["notes", "--ref=ai", "list"]).unwrap_or_default();
    assert!(
        notes_list.contains(&commit.commit_sha),
        "authorship note for the post-switch commit {} must be in refs/notes/ai \
         (the daemon must dispatch on the current notes backend, not the one \
         cached at startup); notes list: {:?}",
        commit.commit_sha,
        notes_list
    );

    // The user-visible behavior: blame in local-only mode shows AI attribution.
    second.assert_committed_lines(lines!["local mode line".ai()]);
}

/// Regression: commits made while connected mode used the `http` backend have
/// their note only in notes-db. Once the machine reads its backend as `both`
/// (connected mode today), the first note write in the repository must copy
/// those notes into refs/notes/ai, so blame stops showing them as unattributed.
#[test]
fn switch_from_http_to_both_backfills_earlier_notes_into_git() {
    let mut repo = TestRepo::new_dedicated_daemon_with_initial_config(|patch| {
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: None,
        });
    });

    // Raw git commit: the harness commit helpers read notes from git, which
    // the http backend does not write.
    let mut first = repo.filename("first.txt");
    first.set_contents(lines!["connected mode line".ai()]);
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "commit while connected over http"])
        .unwrap();
    repo.sync_daemon();
    let first_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(
        repo.read_authorship_note(&first_sha).is_none(),
        "precondition: the http backend must not write refs/notes/ai"
    );

    // Older onboarding wrote `http` for connected mode; today it is `both`.
    repo.patch_autter_config(|patch| {
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Both,
            backend_url: None,
        });
    });

    let mut second = repo.filename("second.txt");
    second.set_contents(lines!["both mode line".ai()]);
    repo.stage_all_and_commit("commit while connected over both")
        .unwrap();
    repo.sync_daemon();

    assert!(
        repo.read_authorship_note(&first_sha).is_some(),
        "the first git-notes write under `both` must backfill the http-era note \
         for {first_sha} into refs/notes/ai"
    );
    first.assert_committed_lines(lines!["connected mode line".ai()]);
    second.assert_committed_lines(lines!["both mode line".ai()]);

    let marker = repo
        .path()
        .join(".git")
        .join("ai")
        .join("notes_backfill_v1");
    assert!(
        marker.exists(),
        "automatic backfill must record that it ran ({})",
        marker.display()
    );

    // Running it explicitly afterwards finds nothing left to copy.
    let output = repo.autter(&["notes", "backfill"]).unwrap();
    assert!(
        output.contains("No notes to backfill"),
        "second backfill must be a no-op, got: {output}"
    );
}

/// `autter notes backfill` copies notes that exist only in notes-db into
/// refs/notes/ai, reports how many it wrote, never overwrites a local note,
/// and writes nothing on a second run.
#[test]
fn notes_backfill_command_writes_missing_notes_and_keeps_local_ones() {
    let repo = TestRepo::new();

    let mut first = repo.filename("first.txt");
    first.set_contents(lines!["first ai line".ai()]);
    let first_sha = repo.stage_all_and_commit("first").unwrap().commit_sha;
    let first_note = repo
        .read_authorship_note(&first_sha)
        .expect("first commit should have a note");

    let mut second = repo.filename("second.txt");
    second.set_contents(lines!["second ai line".ai()]);
    let second_sha = repo.stage_all_and_commit("second").unwrap().commit_sha;
    let second_note = repo
        .read_authorship_note(&second_sha)
        .expect("second commit should have a note");

    // Simulate the http era: the first commit's note lives only in notes-db.
    repo.git_og(&["notes", "--ref=ai", "remove", &first_sha])
        .unwrap();
    assert!(repo.read_authorship_note(&first_sha).is_none());

    let notes_db_path = repo
        .test_home_path()
        .join(".autter")
        .join("internal")
        .join("notes-db");
    std::fs::create_dir_all(notes_db_path.parent().unwrap()).unwrap();
    let mut db = NotesDatabase::open_at_path(&notes_db_path).expect("open notes db");
    db.cache_synced_notes(&[
        (first_sha.clone(), first_note.clone()),
        (second_sha.clone(), "stale notes-db copy".to_string()),
    ])
    .expect("seed notes db");
    drop(db);

    let output = repo.autter(&["notes", "backfill"]).unwrap();
    assert!(
        output.contains("Backfilled 1 authorship note "),
        "expected exactly one note written, got: {output}"
    );

    assert_eq!(
        repo.read_authorship_note(&first_sha)
            .map(|n| n.trim().to_string()),
        Some(first_note.trim().to_string()),
        "the notes-db copy must be written for the commit that had no local note"
    );
    assert_eq!(
        repo.read_authorship_note(&second_sha)
            .map(|n| n.trim().to_string()),
        Some(second_note.trim().to_string()),
        "an existing local note must never be overwritten"
    );
    first.assert_committed_lines(lines!["first ai line".ai()]);
    second.assert_committed_lines(lines!["second ai line".ai()]);

    let again = repo.autter(&["notes", "backfill"]).unwrap();
    assert!(
        again.contains("No notes to backfill"),
        "second run must write nothing, got: {again}"
    );
}
