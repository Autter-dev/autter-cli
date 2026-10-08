//! AI tools autter cannot capture yet must be called out, not silently
//! attributed to the user. Google Antigravity is captured through its hooks
//! (see antigravity.rs); Kiro stands in here for a tool with no capture path.

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
    assert!(
        output.contains("Kiro detected — its edits are not captured"),
        "stats should explain the untracked share: {output}"
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
