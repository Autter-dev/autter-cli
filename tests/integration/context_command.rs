//! `autter context` and `autter memory skills` ask the Autter cloud about the
//! repository, so they must send nothing in local mode or for a repository
//! excluded from sharing. The tests are not logged in: a command that gets
//! past the sharing policy stops at "Sign in", which makes the connected run
//! the control.

use crate::repos::test_repo::TestRepo;

fn repo_with_remote() -> TestRepo {
    let repo = TestRepo::new();
    repo.git_og(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/context-test.git",
    ])
    .expect("add remote");
    repo
}

fn command_error(repo: &TestRepo, args: &[&str]) -> String {
    repo.autter(args)
        .expect_err("the command should fail without sending anything")
}

#[test]
fn context_sends_nothing_in_local_mode() {
    let mut repo = repo_with_remote();
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("local".to_string()));
    for args in [
        &["context", "add retries to the webhook"][..],
        &["memory", "skills", "preview"][..],
        &["memory", "skills", "install"][..],
    ] {
        let output = command_error(&repo, args);
        assert!(output.contains("local mode"), "{output}");
        assert!(!output.contains("Sign in"), "{output}");
    }
}

#[test]
fn context_sends_nothing_for_an_excluded_repository() {
    let mut repo = repo_with_remote();
    repo.patch_autter_config(|patch| {
        patch.prompt_storage = Some("default".to_string());
        patch.exclude_prompts_in_repositories = Some(vec!["https://github.com/acme/*".to_string()]);
    });
    let output = command_error(&repo, &["context", "add retries to the webhook"]);
    assert!(output.contains("excluded from sharing"), "{output}");
}

#[test]
fn context_in_connected_mode_needs_a_login() {
    let mut repo = repo_with_remote();
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("default".to_string()));
    let output = command_error(
        &repo,
        &["context", "add retries", "--path", "src/http/retry.rs"],
    );
    assert!(output.contains("autter login"), "{output}");
    let output = command_error(&repo, &["memory", "skills", "install"]);
    assert!(output.contains("autter login"), "{output}");
}

#[test]
fn context_rejects_bad_usage_before_anything_else() {
    let repo = repo_with_remote();
    let output = command_error(&repo, &["context"]);
    assert!(output.contains("context needs the task"), "{output}");
    let output = command_error(&repo, &["context", "fix it", "--budget", "50"]);
    assert!(output.contains("--budget must be"), "{output}");
    let output = command_error(&repo, &["context", "fix it", "--include", "secrets"]);
    assert!(output.contains("--include must be from"), "{output}");
    let output = command_error(&repo, &["memory", "skills"]);
    assert!(
        output.contains("memory skills needs install or preview"),
        "{output}"
    );
}

#[test]
fn context_paths_must_be_inside_the_repository() {
    let mut repo = repo_with_remote();
    repo.patch_autter_config(|patch| patch.prompt_storage = Some("default".to_string()));
    let output = command_error(&repo, &["context", "fix it", "--path", "../outside.rs"]);
    assert!(output.contains("outside this repository"), "{output}");
}

#[test]
fn mcp_rejects_bad_usage() {
    let repo = repo_with_remote();
    let output = command_error(&repo, &["mcp"]);
    assert!(output.contains("connect or disconnect"), "{output}");
    let output = command_error(&repo, &["mcp", "connect", "--agent", "windsurf"]);
    assert!(output.contains("unknown agent windsurf"), "{output}");
}
