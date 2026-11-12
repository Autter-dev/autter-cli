//! AI tools autter cannot capture yet must be called out, not silently
//! attributed to the user. Google Antigravity is captured through its hooks
//! (see antigravity.rs); Kiro stands in here for a tool with no capture path.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn mark_kiro_installed(repo: &TestRepo) {
    fs::create_dir_all(repo.test_home_path().join(".kiro")).unwrap();
}

#[test]
fn doctor_warns_when_an_unsupported_agent_is_installed() {
    let repo = TestRepo::new();
    mark_kiro_installed(&repo);

    let raw = repo
        .autter(&["doctor", "--json"])
        .unwrap_or_else(|output| output);
    let start = raw.find('{').expect("doctor prints JSON");
    let report: serde_json::Value = serde_json::from_str(raw[start..].trim()).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "Kiro")
        .unwrap_or_else(|| panic!("doctor should report Kiro: {raw}"));
    assert_eq!(check["status"], "warning", "{raw}");
    assert_eq!(
        check["summary"],
        "Kiro detected — its edits are not captured; they will be attributed to you",
        "{raw}"
    );
    assert!(
        check["details"]
            .to_string()
            .contains("docs/unsupported-agents.md"),
        "warning should point to the tracking doc: {raw}"
    );
}

#[test]
fn stats_notes_unsupported_agent_when_commit_is_untracked() {
    let repo = TestRepo::new();
    mark_kiro_installed(&repo);

    // A commit with no checkpoints: 100% untracked, as with uncaptured edits.
    fs::write(repo.path().join("generated.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    repo.stage_all_and_commit("edit made by an uncaptured agent")
        .unwrap();

    let output = repo.autter(&["stats"]).expect("stats should succeed");
    // Other uncapturable tools installed on the machine running the tests
    // (found under /Applications or on PATH) are named in the same note, which
    // then reads "Kiro, Zed (agent panel) detected — autter cannot capture…".
    let note = output
        .lines()
        .find(|line| line.starts_with("Note: "))
        .unwrap_or_else(|| panic!("stats should explain the untracked share: {output}"));
    assert!(
        note.contains("Kiro") && note.contains("detected") && note.contains("capture"),
        "the note should name Kiro as not captured: {output}"
    );
}

#[test]
fn gemini_cli_home_does_not_trigger_an_antigravity_warning() {
    // ~/.gemini exists for Gemini CLI users (extensions/ and settings.json).
    let repo = TestRepo::new();
    let gemini = repo.test_home_path().join(".gemini");
    fs::create_dir_all(gemini.join("extensions")).unwrap();
    fs::write(gemini.join("settings.json"), "{}").unwrap();

    let raw = repo
        .autter(&["doctor", "--json"])
        .unwrap_or_else(|output| output);
    assert!(!raw.contains("Antigravity detected"), "{raw}");
}

/// With an AI tool autter cannot capture installed, a recorded commit with no
/// agent lines may have been written by it. `stats` must not say "no agent
/// wrote its lines" right above "If it wrote this code…".
#[test]
fn stats_does_not_rule_out_an_agent_when_an_unsupported_one_is_detected() {
    let repo = TestRepo::new();
    mark_kiro_installed(&repo);

    fs::write(repo.path().join("generated.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    repo.stage_all_and_commit("edit made by an uncaptured agent")
        .unwrap();
    let mut file = repo.filename("generated.rs");
    file.assert_committed_lines(crate::lines![
        "fn a() {}".unattributed_human(),
        "fn b() {}".unattributed_human(),
    ]);

    let output = repo.autter(&["stats"]).expect("stats should succeed");
    assert!(
        output.contains("If it wrote this code")
            || output.contains("If one of them wrote this code"),
        "the note should be printed: {output}"
    );
    assert!(
        !output.contains("no agent wrote its lines"),
        "stats must not rule out an agent it cannot see: {output}"
    );
    assert!(
        output.contains("none of the agents it captures wrote its lines"),
        "{output}"
    );
}

/// The note single-commit `stats` prints is also printed for a range and, once,
/// by `autter log`.
#[test]
fn range_stats_and_log_note_an_unsupported_agent() {
    let repo = TestRepo::new();
    mark_kiro_installed(&repo);

    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut base = repo.filename("base.txt");
    base.assert_committed_lines(crate::lines!["base".unattributed_human()]);

    fs::write(repo.path().join("one.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    repo.stage_all_and_commit("first uncaptured edit").unwrap();
    let mut one = repo.filename("one.rs");
    one.assert_committed_lines(crate::lines![
        "fn a() {}".unattributed_human(),
        "fn b() {}".unattributed_human(),
    ]);

    fs::write(repo.path().join("two.rs"), "fn c() {}\n").unwrap();
    repo.stage_all_and_commit("second uncaptured edit").unwrap();
    let mut two = repo.filename("two.rs");
    two.assert_committed_lines(crate::lines!["fn c() {}".unattributed_human()]);

    let is_note = |line: &str| {
        let line = line.trim_start();
        line.starts_with("Note: ") && line.contains("Kiro") && line.contains("capture")
    };

    let range = repo
        .autter(&["stats", "HEAD~2..HEAD"])
        .expect("range stats should succeed");
    assert_eq!(
        range.lines().filter(|line| is_note(line)).count(),
        1,
        "range stats should explain the untracked share: {range}"
    );

    let log = repo
        .autter(&["log", "-n", "3"])
        .expect("log should succeed");
    assert_eq!(
        log.lines().filter(|line| is_note(line)).count(),
        1,
        "log should name the uncaptured agent once, not under every commit: {log}"
    );
}
