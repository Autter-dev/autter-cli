#![allow(dead_code)]

use std::path::PathBuf;

/// Get the path to a test fixture file
///
/// # Example
/// ```no_run
/// use test_utils::fixture_path;
///
/// let path = fixture_path("example.json");
/// // Returns: /path/to/project/tests/fixtures/example.json
/// ```
pub fn fixture_path(filename: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/")).join(filename)
}

/// Load the contents of a test fixture file as a string
///
/// # Example
/// ```no_run
/// use test_utils::load_fixture;
///
/// let contents = load_fixture("example.json");
/// // Returns the string contents of tests/fixtures/example.json
/// ```
///
/// # Panics
/// Panics if the fixture file cannot be read
pub fn load_fixture(filename: &str) -> String {
    std::fs::read_to_string(fixture_path(filename))
        .unwrap_or_else(|_| panic!("Failed to read fixture: {}", filename))
}

/// Assert an authorship note is in the current sessions format.
///
/// `metadata.sessions` is the canonical record. `metadata.prompts` is *not*
/// expected to be empty: `authorship::post_commit` deliberately mirrors every
/// session into it (keyed by the same `s_…` id) so readers that predate
/// sessions keep working. So "this note has no prompts" is the wrong invariant —
/// "this note has no prompts outside that mirror" is the right one, and it still
/// catches a legacy prompt hash leaking into a sessions-format note.
///
/// Pair with an `assert!(!log.metadata.sessions.is_empty(), ..)` to also require
/// that the note actually carries AI sessions.
pub fn assert_sessions_canonical(
    log: &autter::authorship::authorship_log_serialization::AuthorshipLog,
) {
    for (entry_id, prompt) in &log.metadata.prompts {
        let session = log.metadata.sessions.get(entry_id).unwrap_or_else(|| {
            panic!(
                "prompts entry {entry_id} has no matching session: legacy prompt hashes \
                 must not appear in a sessions-format note (prompts: {:?})",
                prompt.agent_id
            )
        });
        assert_eq!(
            &prompt.agent_id, &session.agent_id,
            "prompts[{entry_id}] must mirror its session's agent_id"
        );
    }
}

/// Same contract as [`assert_sessions_canonical`], for the `prompts`/`sessions`
/// maps in `autter diff --json` output. `diff` merges both maps straight from the
/// authorship note, so a sessions-format commit legitimately shows the mirror in
/// `prompts` too; what must never appear is a prompt key with no session.
pub fn assert_json_sessions_canonical(json: &serde_json::Value) {
    let empty = serde_json::Map::new();
    let prompts = json["prompts"].as_object().unwrap_or(&empty);
    let sessions = json["sessions"].as_object().unwrap_or(&empty);
    for key in prompts.keys() {
        assert!(
            sessions.contains_key(key),
            "diff json prompts entry {key} has no matching session: legacy prompt \
             hashes must not appear in a sessions-format note"
        );
    }
}
