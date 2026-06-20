use crate::repos::test_repo::TestRepo;
use std::fs;

#[test]
fn test_file_changes_tracks_checkpointed_files() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("hot.rs");
    fs::write(&file_path, "line1\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    fs::write(&file_path, "line1\nline2\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "hot.rs"])
        .unwrap();

    fs::write(&file_path, "line1\nline2\nline3\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "hot.rs"])
        .unwrap();

    let output = repo
        .autter(&["file-changes", "--json"])
        .expect("file-changes command should succeed");

    let json: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
    let files = json["files"].as_array().expect("files array");
    assert!(!files.is_empty());
    assert_eq!(files[0]["path"], "hot.rs");
    assert!(files[0]["change_count"].as_u64().unwrap() >= 2);
}
