//! AI tools autter cannot capture yet must be called out, not silently
//! attributed to the user.

use crate::repos::test_repo::TestRepo;
use std::fs;

fn mark_antigravity_installed(repo: &TestRepo) {
    // Antigravity keeps its agent state under ~/.gemini/antigravity.
    fs::create_dir_all(repo.test_home_path().join(".gemini").join("antigravity")).unwrap();
}

#[test]
fn doctor_warns_when_antigravity_is_installed() {
    let repo = TestRepo::new();
    mark_antigravity_installed(&repo);

    let raw = repo
        .autter(&["doctor", "--json"])
        .unwrap_or_else(|output| output);
    let start = raw.find('{').expect("doctor prints JSON");
    let report: serde_json::Value = serde_json::from_str(raw[start..].trim()).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "Google Antigravity")
        .unwrap_or_else(|| panic!("doctor should report Antigravity: {raw}"));
    assert_eq!(check["status"], "warning", "{raw}");
    assert_eq!(
        check["summary"],
        "Google Antigravity detected — its edits are not captured; they will be attributed to you",
        "{raw}"
    );
}

#[test]
fn stats_notes_unsupported_agent_when_commit_is_untracked() {
    let repo = TestRepo::new();
    mark_antigravity_installed(&repo);

    // A commit with no checkpoints: 100% untracked, as with Antigravity edits.
    fs::write(repo.path().join("generated.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    repo.stage_all_and_commit("edit made by an uncaptured agent")
        .unwrap();

    let output = repo.autter(&["stats"]).expect("stats should succeed");
    assert!(
        output.contains("Google Antigravity detected — its edits are not captured"),
        "stats should explain the untracked share: {output}"
    );
}

#[test]
fn stats_has_no_notice_without_unsupported_agents() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("plain.rs"), "fn a() {}\n").unwrap();
    repo.stage_all_and_commit("plain commit").unwrap();
    let output = repo.autter(&["stats"]).expect("stats should succeed");
    // Only meaningful when the machine itself has none of these apps
    // installed system-wide (e.g. /Applications/Antigravity.app on macOS).
    if !std::path::Path::new("/Applications/Antigravity.app").exists() {
        assert!(!output.contains("Google Antigravity detected"), "{output}");
    }
}
