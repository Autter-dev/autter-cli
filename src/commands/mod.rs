pub mod autter_handlers;
pub mod blame;
pub mod checkpoint_agent;
pub mod ci_handlers;
pub mod config;
pub mod daemon;
pub mod debug;
pub mod diff;
pub mod exchange_nonce;
pub mod fetch_notes;
pub mod file_changes;
pub mod flush_metrics_db;
pub mod git_handlers;
pub mod git_hook_handlers;
pub mod hooks;
pub mod install_hooks;
pub mod log;
pub mod login;
pub mod logout;
pub mod notes_migrate;
pub mod onboard;
pub mod personal_dashboard;
pub mod show;
pub mod show_prompt;
pub mod squash_authorship;
pub mod status;
pub mod telemetry;
pub mod upgrade;
pub mod whoami;

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
