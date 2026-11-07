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
fn onboard_local_with_consent_succeeds_cleanly_when_every_step_works() {
    let repo = TestRepo::new();
    // `--yes` is consent to git capture and the background service, the only
    // way a non-interactive onboarding sets them up.
    let output = repo
        .autter(&["onboard", "--local", "--yes", "--no-telemetry"])
        .expect("clean onboarding exits 0");
    assert!(output.contains("You're all set!"), "{output}");
    assert!(output.contains("Capture verified"), "{output}");
    assert!(!output.contains("Set up with warnings"), "{output}");
}

/// Reported on Linux, Windows and macOS: a non-interactive onboarding (an
/// installer, `onboard --local --force` from a script) skipped git capture
/// and still ended with "You're all set!", then `autter doctor` failed.
#[test]
fn onboard_without_git_capture_never_says_all_set() {
    let repo = TestRepo::new();
    let result = repo.autter(&["onboard", "--local", "--no-telemetry"]);
    let output = match &result {
        Ok(out) | Err(out) => out.clone(),
    };
    assert!(
        result.is_err(),
        "onboarding that left capture off must exit non-zero: {output}"
    );
    assert!(!output.contains("You're all set"), "{output}");
    assert!(!output.contains("Capture verified"), "{output}");
    assert!(output.contains("Set up with warnings"), "{output}");
    assert!(
        output.contains("Git capture and the background service are not set up"),
        "{output}"
    );
    assert!(output.contains("Fix: autter install --system"), "{output}");
    // The mode choice itself was saved; only capture is unfinished.
    assert!(
        output.contains("Your choice (local mode) is saved"),
        "{output}"
    );

    // Re-running with --force and no consent is the same: never "all set".
    let rerun = repo
        .autter(&["onboard", "--local", "--force", "--no-telemetry"])
        .unwrap_or_else(|e| e);
    assert!(!rerun.contains("You're all set"), "{rerun}");
    assert!(rerun.contains("Fix: autter install --system"), "{rerun}");
}
