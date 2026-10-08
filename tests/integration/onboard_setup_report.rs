//! Onboarding must not claim "You're all set!" when a step failed.

use crate::repos::test_repo::TestRepo;
use std::fs;

#[test]
fn onboard_reports_warnings_and_fails_when_the_choice_cannot_be_saved() {
    let repo = TestRepo::new();

    // Make ~/.autter/config.json unwritable by putting a directory there.
    let config_path = repo.test_home_path().join(".autter").join("config.json");
    let _ = fs::remove_file(&config_path);
    fs::create_dir_all(&config_path).unwrap();

    let result = repo.autter(&["onboard", "--local", "--no-telemetry"]);
    let output = match &result {
        Ok(out) | Err(out) => out.clone(),
    };
    assert!(
        result.is_err(),
        "a degraded onboarding must exit non-zero: {output}"
    );
    assert!(
        output.contains("Set up with warnings"),
        "should end with the warnings summary: {output}"
    );
    assert!(
        output.contains("Fix: autter onboard --force"),
        "should name the exact fix command: {output}"
    );
    assert!(
        !output.contains("You're all set"),
        "must not claim success after a failure: {output}"
    );
}

#[test]
fn onboard_local_succeeds_cleanly_when_every_step_works() {
    let repo = TestRepo::new();
    let output = repo
        .autter(&["onboard", "--local", "--no-telemetry"])
        .expect("clean onboarding exits 0");
    assert!(output.contains("You're all set!"), "{output}");
    assert!(!output.contains("Set up with warnings"), "{output}");
}
