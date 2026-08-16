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
