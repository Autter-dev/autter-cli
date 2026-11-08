pub mod arg_parser;
pub mod ask;
pub mod autter_handlers;
pub mod badge;
pub mod blame;
pub mod checkpoint_agent;
pub mod ci_handlers;
pub mod config;
pub mod context;
pub mod coverage;
pub mod daemon;
pub mod debug;
pub mod diff;
pub mod doctor;
pub mod exchange_nonce;
pub mod fetch_notes;
pub mod file_changes;
pub mod flush_metrics_db;
pub mod git_handlers;
pub mod git_hook_handlers;
pub mod home;
pub mod hooks;
pub mod install_hooks;
pub mod interactive;
pub mod log;
pub mod login;
pub mod logout;
pub mod mcp;
pub mod memory;
pub mod memory_hook;
pub mod memory_skills;
pub mod notes_backfill;
pub mod notes_migrate;
pub mod onboard;
pub mod personal_dashboard;
pub mod show;
pub mod show_prompt;
pub mod squash_authorship;
pub mod status;
pub mod sync;
pub mod telemetry;
pub mod upgrade;
pub mod usage_hints;
pub mod whoami;
pub mod wrapped;

/// Process exit codes for `autter` subcommands.
///
/// Direct, user-invoked subcommands (run by people, scripts, or CI) follow the
/// convention used by most CLIs and by git itself:
///
/// * [`EXIT_SUCCESS`] (`0`) — the command completed successfully.
/// * [`EXIT_RUNTIME_ERROR`] (`1`) — a runtime/IO/lookup error (e.g. repository
///   not found, a git invocation failed, serialization failed).
/// * [`EXIT_USAGE_ERROR`] (`2`) — the caller misused the command (missing or
///   unknown subcommand, missing required argument, unparseable flag value).
///
/// This policy intentionally does NOT apply to the git proxy/hook path or to
/// the `checkpoint` command. Those run *inside* the user's `git` invocation or
/// an AI agent's editor hook, where a non-zero exit would break the host
/// process, so they deliberately exit `0` even when they fail internally. See
/// `autter_handlers::handle_checkpoint` for the rationale.
pub const EXIT_SUCCESS: i32 = 0;
/// Runtime/IO error in a user-invoked subcommand. See [`EXIT_SUCCESS`].
pub const EXIT_RUNTIME_ERROR: i32 = 1;
/// Usage/argument error in a user-invoked subcommand. See [`EXIT_SUCCESS`].
pub const EXIT_USAGE_ERROR: i32 = 2;

/// The error a repository-scoped command prints when it can't open the
/// repository. Outside a repository that is one plain sentence instead of
/// git's `rev-parse` failure.
pub fn repository_error_message(command: &str, error: &crate::error::AutterError) -> String {
    let text = error.to_string();
    if text.contains("not a git repository") {
        format!(
            "error: `autter {command}` must be run inside a git repository (or pass -C <path>)."
        )
    } else {
        format!("error: could not open the git repository: {text}")
    }
}

/// Print [`repository_error_message`] and exit with [`EXIT_RUNTIME_ERROR`].
pub fn exit_repository_error(command: &str, error: &crate::error::AutterError) -> ! {
    eprintln!("{}", repository_error_message(command, error));
    std::process::exit(EXIT_RUNTIME_ERROR);
}

/// Print a one-line stderr pointer to `autter doctor`.
///
/// Reserved for error paths that usually mean a broken installation
/// (unreachable background service, failed hook install, unrunnable git
/// binary) rather than a bad invocation -- `autter doctor` runs the
/// setup checks and prints a `fix:` line for each failure.
pub fn suggest_autter_doctor() {
    eprintln!(
        "[autter] run `autter doctor` to check your setup -- each failed check includes a suggested fix"
    );
}

#[cfg(test)]
mod repository_error_tests {
    use super::*;

    #[test]
    fn outside_a_repository_is_one_plain_sentence() {
        let error = crate::error::AutterError::GitCliError {
            code: Some(128),
            stderr: "fatal: not a git repository (or any of the parent directories): .git"
                .to_string(),
            args: vec!["rev-parse".to_string()],
        };
        assert_eq!(
            repository_error_message("stats", &error),
            "error: `autter stats` must be run inside a git repository (or pass -C <path>)."
        );
    }
}
