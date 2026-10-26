//! `autter memory` reads repository memory from the Autter cloud, so it must
//! send nothing in local mode or for a repository excluded from sharing. The
//! tests are not logged in: a command that gets past the sharing policy stops
//! at "Sign in", which makes the connected run the control.

use crate::repos::test_repo::TestRepo;

fn repo_with_remote() -> TestRepo {
    let repo = TestRepo::new();
    repo.git_og(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/memory-test.git",
    ])
    .expect("add remote");
    repo
}

fn memory_error(repo: &TestRepo, args: &[&str]) -> String {
    repo.autter(args)
        .expect_err("autter memory should fail without sending anything")
}

#[test]
fn memory_search_sends_nothing_in_local_mode() {
    let mut repo = repo_with_remote();
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("local".to_string()));
    for args in [
        &["memory", "search", "how do we retry"][..],
        &["memory", "show"][..],
    ] {
        let output = memory_error(&repo, args);
        assert!(output.contains("local mode"), "{output}");
        assert!(!output.contains("Sign in"), "{output}");
    }
}

#[test]
fn memory_search_sends_nothing_for_an_excluded_repository() {
    let mut repo = repo_with_remote();
    repo.patch_autter_config(|patch| {
        patch.prompt_storage = Some("default".to_string());
        patch.exclude_prompts_in_repositories = Some(vec!["https://github.com/acme/*".to_string()]);
    });
    let output = memory_error(&repo, &["memory", "search", "how do we retry"]);
    assert!(output.contains("excluded from sharing"), "{output}");
    // An explicit --repo is checked against the same policy.
    let output = memory_error(
        &repo,
        &["memory", "show", "--repo", "git@github.com:acme/other.git"],
    );
    assert!(output.contains("excluded from sharing"), "{output}");
}

#[test]
fn memory_search_in_connected_mode_needs_a_login() {
    let mut repo = repo_with_remote();
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("default".to_string()));
    let output = memory_error(&repo, &["memory", "search", "how do we retry"]);
    assert!(output.contains("autter login"), "{output}");
}

#[test]
fn memory_rejects_bad_usage_before_anything_else() {
    let repo = repo_with_remote();
    let output = memory_error(&repo, &["memory", "search", "retry", "--kind", "secrets"]);
    assert!(output.contains("--kind must be one of"), "{output}");
    let output = memory_error(&repo, &["memory"]);
    assert!(output.contains("memory needs a subcommand"), "{output}");
}

#[test]
fn memory_for_files_sends_nothing_in_local_mode() {
    let mut repo = repo_with_remote();
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("local".to_string()));
    for args in [
        &["memory", "for", "src/lib.rs"][..],
        &["memory", "for"][..],
        &["memory", "areas"][..],
        &["memory", "ledger"][..],
    ] {
        let output = memory_error(&repo, args);
        assert!(output.contains("local mode"), "{output}");
    }
}

#[test]
fn memory_for_without_changes_asks_for_files() {
    let mut repo = repo_with_remote();
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("default".to_string()));
    let output = memory_error(&repo, &["memory", "for"]);
    assert!(
        output.contains("Nothing changed in this checkout"),
        "{output}"
    );
    let output = memory_error(&repo, &["memory", "for", "../outside.rs"]);
    assert!(output.contains("outside this repository"), "{output}");
}
