use autter::auth;
use autter::commands;
use autter::utils::{SuperuserCheckResult, check_superuser_guard, print_superuser_warning};
use clap::Parser;

#[derive(Parser)]
#[command(name = "autter")]
#[command(about = "git proxy with AI authorship tracking", long_about = None)]
#[command(disable_help_flag = true, disable_version_flag = true)]
struct Cli {
    /// Git command and arguments
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn is_superuser_exempt_command(args: &[String]) -> bool {
    let first = match args.first() {
        Some(a) => a.as_str(),
        None => return true,
    };
    matches!(
        first,
        "help"
            | "--help"
            | "-h"
            | "version"
            | "--version"
            | "-v"
            | "upgrade"
            | "debug"
            | "uninstall-hooks"
    ) || (first == "bg" || first == "d" || first == "daemon")
        && args
            .get(1)
            .is_some_and(|s| s == "run" || s == "status" || s == "shutdown")
}

fn main() {
    // Get the binary name that was called
    let binary_name = std::env::args_os()
        .next()
        .and_then(|arg| arg.into_string().ok())
        .and_then(|path| {
            std::path::Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or("autter".to_string());

    if commands::git_hook_handlers::is_git_hook_binary_name(&binary_name) {
        eprintln!(
            "autter: the git core hooks feature has been sunset.\n\
             To remove the deprecated autter hook symlinks from this repository, run:\n\
             \n\
             \x20 autter git-hooks remove\n"
        );
        std::process::exit(0);
    }

    // Report unexpected panics as errors (surfaces in PostHog Error Tracking
    // via the daemon when the user has opted into telemetry). Best-effort and
    // preserves normal panic-to-stderr behavior.
    autter::observability::install_panic_hook();

    let cli = Cli::parse();

    #[cfg(debug_assertions)]
    {
        if std::env::var("AUTTER").as_deref() == Ok("git") {
            commands::git_handlers::handle_git(&cli.args);
            return;
        }
    }

    if binary_name == "autter" || binary_name == "autter.exe" {
        // Exit quietly when a reader closes our output pipe early (e.g.
        // `autter blame | head`) instead of crashing with a print panic. Only
        // the direct CLI opts in; the git proxy keeps its own panic recovery
        // and the daemon opts back out in `run_daemon`.
        autter::observability::set_exit_quietly_on_broken_pipe(true);
        // Block elevated privileges to prevent creating root-owned files
        // that break normal-user daemon startup. Only applies to direct
        // `autter` commands (not the git proxy, which must stay transparent).
        // Exempt commands that must work regardless (upgrade, daemon run, help, etc.).
        if !is_superuser_exempt_command(&cli.args) {
            match check_superuser_guard() {
                SuperuserCheckResult::WarnFutureBlock => print_superuser_warning(),
                // Opted in with AUTTER_ALLOW_SUPERUSER=1 or `allow_superuser`:
                // the user said they know, so stay quiet. This used to print a
                // warning line on every command anyway.
                SuperuserCheckResult::AllowedWithWarning | SuperuserCheckResult::Allowed => {}
            }
        }
        // Warn if the CLI is below the platform's minimum required version.
        // Skip exempt commands (help, version, upgrade, daemon control) where the
        // warning would be noise or redundant.
        if !is_superuser_exempt_command(&cli.args) {
            commands::upgrade::maybe_warn_below_min_version();
        }
        // Remind the user when their login has expired (cloud sync paused),
        // except on commands where it would be noise or redundant.
        let first_arg = cli.args.first().map(String::as_str);
        if !is_superuser_exempt_command(&cli.args)
            && !matches!(
                first_arg,
                Some("login" | "logout" | "onboard" | "checkpoint")
            )
        {
            auth::notice::maybe_warn_logged_out();
        }
        // Setup not finished, an agent installed since onboarding, or (npm
        // installs) a newer release: one line each, only for a person at a
        // terminal on an everyday command.
        commands::setup_notices::maybe_print(&cli.args);
        // First command after an upgrade: refresh the hooks of agents the user
        // already set up, in the background, so no hook keeps calling a binary
        // or preset from the previous version.
        autter::mdm::hook_refresh::maybe_refresh_after_upgrade(&cli.args);
        commands::autter_handlers::handle_autter(&cli.args);
        std::process::exit(0);
    }

    commands::git_handlers::handle_git(&cli.args);
}
