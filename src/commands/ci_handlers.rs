use crate::ci::ci_context::{CiContext, CiEvent, CiRunOptions, CiRunResult};
use crate::ci::github::{get_github_ci_context, install_github_ci_workflow};
use crate::ci::gitlab::{get_gitlab_ci_context, print_gitlab_ci_yaml};
use crate::git::repository::find_repository_in_path;

/// Print a human-readable message for a CiRunResult
fn print_ci_result(result: &CiRunResult, prefix: &str) {
    match result {
        CiRunResult::AuthorshipRewritten { .. } => {
            println!("{}: authorship rewritten successfully", prefix);
        }
        CiRunResult::SyncAuthorshipRewritten { .. } => {
            println!("{}: authorship rewritten successfully", prefix);
        }
        CiRunResult::AlreadyExists { .. } => {
            println!("{}: authorship already exists", prefix);
        }
        CiRunResult::SkippedSimpleMerge => {
            println!("{}: skipped simple merge (authorship preserved)", prefix);
        }
        CiRunResult::ForkNotesPreserved => {
            println!("{}: fork notes preserved", prefix);
        }
        CiRunResult::SkippedFastForward => {
            println!("{}: skipped fast-forward merge", prefix);
        }
        CiRunResult::SkippedNonRebaseSync => {
            println!("{}: skipped non-rebase PR sync", prefix);
        }
        CiRunResult::SkippedExistingSyncNotes => {
            println!("{}: skipped PR sync with existing current notes", prefix);
        }
        CiRunResult::NoAuthorshipAvailable => {
            println!(
                "{}: no AI authorship to track (pre-autter commits or human-only code)",
                prefix
            );
        }
    }
}

/// True when `arg` asks for help. `help` counts only as the first token of a
/// (sub)command; `--help`/`-h` count in any position.
fn is_help_flag(arg: &str) -> bool {
    arg == "--help" || arg == "-h"
}

/// True when the args of a `ci` (sub)command ask for help anywhere. Checked
/// before any work so `--help` never writes files, fetches, or pushes.
fn wants_help(args: &[String]) -> bool {
    args.first().is_some_and(|a| a == "help") || args.iter().any(|a| is_help_flag(a))
}

/// How a help screen was reached: an explicit request prints to stdout and
/// exits 0; a usage error prints to stderr and exits with the usage code.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HelpReason {
    Requested,
    UsageError,
}

fn print_help_and_exit(text: &str, reason: HelpReason) -> ! {
    match reason {
        HelpReason::Requested => {
            print!("{text}");
            std::process::exit(0);
        }
        HelpReason::UsageError => {
            eprint!("{text}");
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    }
}

pub fn handle_ci(args: &[String]) {
    if args.is_empty() {
        print_help_and_exit(CI_HELP, HelpReason::UsageError);
    }
    if args[0] == "help" || is_help_flag(&args[0]) {
        print_help_and_exit(CI_HELP, HelpReason::Requested);
    }

    match args[0].as_str() {
        "github" => {
            handle_ci_github(&args[1..]);
        }
        "gitlab" => {
            handle_ci_gitlab(&args[1..]);
        }
        "local" => {
            handle_ci_local(&args[1..]);
        }
        _ => {
            eprintln!("Unknown ci subcommand: {}", args[0]);
            print_help_and_exit(CI_HELP, HelpReason::UsageError);
        }
    }
}

fn handle_ci_github(args: &[String]) {
    if args.is_empty() {
        print_help_and_exit(CI_GITHUB_HELP, HelpReason::UsageError);
    }
    if wants_help(args) {
        let text = match args[0].as_str() {
            "install" => CI_GITHUB_INSTALL_HELP,
            "run" => CI_GITHUB_RUN_HELP,
            _ => CI_GITHUB_HELP,
        };
        print_help_and_exit(text, HelpReason::Requested);
    }
    // Subcommands: install | (default: run in CI context)
    match args[0].as_str() {
        "run" => {
            let no_cleanup = args[1..].iter().any(|a| a == "--no-cleanup");
            let ci_context = get_github_ci_context();
            match ci_context {
                Ok(Some(ci_context)) => {
                    tracing::debug!("GitHub CI context: {:?}", ci_context);
                    match ci_context.run() {
                        Ok(result) => {
                            tracing::debug!("GitHub CI result: {:?}", result);
                            print_ci_result(&result, "GitHub CI");
                        }
                        Err(e) => {
                            eprintln!("Error running GitHub CI context: {}", e);
                            std::process::exit(1);
                        }
                    }
                    if !no_cleanup {
                        if let Err(e) = ci_context.teardown() {
                            eprintln!("Error tearing down GitHub CI context: {}", e);
                            std::process::exit(1);
                        }
                        tracing::debug!("GitHub CI context teared down");
                    } else {
                        tracing::debug!("Skipping teardown (--no-cleanup)");
                    }
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Failed to get GitHub CI context: {}", e);
                    std::process::exit(1);
                }
                Ok(None) => {
                    // No actionable pull_request event for autter. This is not
                    // an error, especially now that synchronize events run for
                    // every PR head update.
                    println!("No GitHub CI context found; nothing to do");
                    std::process::exit(0);
                }
            }
        }
        "install" => match install_github_ci_workflow() {
            Ok(path) => {
                println!("Installed GitHub Actions workflow to {}", path.display());
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("Failed to install GitHub CI workflow: {}", e);
                std::process::exit(1);
            }
        },
        other => {
            eprintln!("Unknown ci github subcommand: {}", other);
            print_help_and_exit(CI_GITHUB_HELP, HelpReason::UsageError);
        }
    }
}

fn handle_ci_gitlab(args: &[String]) {
    if args.is_empty() {
        print_help_and_exit(CI_GITLAB_HELP, HelpReason::UsageError);
    }
    if wants_help(args) {
        let text = match args[0].as_str() {
            "install" => CI_GITLAB_INSTALL_HELP,
            "run" => CI_GITLAB_RUN_HELP,
            _ => CI_GITLAB_HELP,
        };
        print_help_and_exit(text, HelpReason::Requested);
    }
    // Subcommands: install | run
    match args[0].as_str() {
        "run" => {
            let no_cleanup = args[1..].iter().any(|a| a == "--no-cleanup");
            let ci_context = get_gitlab_ci_context();
            match ci_context {
                Ok(Some(ci_context)) => {
                    tracing::debug!("GitLab CI context: {:?}", ci_context);
                    match ci_context.run() {
                        Ok(result) => {
                            tracing::debug!("GitLab CI result: {:?}", result);
                            print_ci_result(&result, "GitLab CI");
                        }
                        Err(e) => {
                            eprintln!("Error running GitLab CI context: {}", e);
                            std::process::exit(1);
                        }
                    }
                    if !no_cleanup {
                        if let Err(e) = ci_context.teardown() {
                            eprintln!("Error tearing down GitLab CI context: {}", e);
                            std::process::exit(1);
                        }
                        tracing::debug!("GitLab CI context teared down");
                    } else {
                        tracing::debug!("Skipping teardown (--no-cleanup)");
                    }
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Failed to get GitLab CI context: {}", e);
                    std::process::exit(1);
                }
                Ok(None) => {
                    // No matching MR found - this is not an error, just nothing to do
                    std::process::exit(0);
                }
            }
        }
        "install" => {
            print_gitlab_ci_yaml();
            std::process::exit(0);
        }
        other => {
            eprintln!("Unknown ci gitlab subcommand: {}", other);
            print_help_and_exit(CI_GITLAB_HELP, HelpReason::UsageError);
        }
    }
}

fn handle_ci_local(args: &[String]) {
    if args.is_empty() {
        print_help_and_exit(CI_LOCAL_HELP, HelpReason::UsageError);
    }
    if wants_help(args) {
        print_help_and_exit(CI_LOCAL_HELP, HelpReason::Requested);
    }

    let event = args[0].as_str();
    let event_args: &[String] = &args[1..];
    let has_bool_flag = |name: &str| event_args.iter().any(|arg| arg == name);

    // Simple flag parser over remaining args: --key value
    let flag = |name: &str| -> Option<String> {
        let mut i = 0usize;
        while i < event_args.len() {
            if event_args[i] == name {
                if i + 1 < event_args.len() {
                    return Some(event_args[i + 1].clone());
                } else {
                    eprintln!("Missing value for flag {}", name);
                    std::process::exit(crate::commands::EXIT_USAGE_ERROR);
                }
            }
            i += 1;
        }
        None
    };

    // Open current repo
    let repo = match find_repository_in_path(".") {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to open repository in current directory: {}", e);
            std::process::exit(1);
        }
    };

    match event {
        "merge" => {
            let skip_fetch_all = has_bool_flag("--skip-fetch");
            let skip_fetch_notes = skip_fetch_all || has_bool_flag("--skip-fetch-notes");
            let skip_fetch_base = skip_fetch_all || has_bool_flag("--skip-fetch-base");
            let skip_fetch_fork_notes = skip_fetch_all || has_bool_flag("--skip-fetch-fork-notes");
            let skip_push = has_bool_flag("--skip-push");

            // Required inputs for merge
            let merge_commit_sha = match flag("--merge-commit-sha") {
                Some(v) => v,
                None => {
                    eprintln!("--merge-commit-sha is required");
                    std::process::exit(crate::commands::EXIT_USAGE_ERROR);
                }
            };

            let base_ref = match flag("--base-ref") {
                Some(v) => v,
                None => {
                    eprintln!("--base-ref is required (e.g., main)");
                    std::process::exit(crate::commands::EXIT_USAGE_ERROR);
                }
            };

            // All flags required for merge
            let head_ref = match flag("--head-ref") {
                Some(v) => v,
                None => {
                    eprintln!("--head-ref is required");
                    std::process::exit(crate::commands::EXIT_USAGE_ERROR);
                }
            };

            let head_sha = match flag("--head-sha") {
                Some(v) => v,
                None => {
                    eprintln!("--head-sha is required");
                    std::process::exit(crate::commands::EXIT_USAGE_ERROR);
                }
            };

            let base_sha = match flag("--base-sha") {
                Some(v) => v,
                None => {
                    eprintln!("--base-sha is required");
                    std::process::exit(crate::commands::EXIT_USAGE_ERROR);
                }
            };

            let fork_clone_url = flag("--fork-clone-url");

            let ctx = CiContext {
                repo,
                event: CiEvent::Merge {
                    merge_commit_sha,
                    head_ref,
                    head_sha,
                    base_ref,
                    base_sha,
                    fork_clone_url,
                },
                // Not used for local runs; teardown not invoked
                temp_dir: std::path::PathBuf::from("."),
            };

            tracing::debug!("Local CI context: {:?}", ctx);
            match ctx.run_with_options(CiRunOptions {
                skip_fetch_notes,
                skip_fetch_base,
                skip_fetch_fork_notes,
                skip_fetch_sync_refs: false,
                skip_push,
            }) {
                Ok(result) => {
                    tracing::debug!("Local CI result: {:?}", result);
                    print_ci_result(&result, "Local CI (merge)");
                }
                Err(e) => {
                    eprintln!("Error running local CI: {}", e);
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
        "sync" | "rebase" => {
            let skip_fetch_all = has_bool_flag("--skip-fetch");
            let skip_fetch_notes = skip_fetch_all || has_bool_flag("--skip-fetch-notes");
            let skip_fetch_sync_refs = skip_fetch_all || has_bool_flag("--skip-fetch-sync-refs");
            let skip_push = has_bool_flag("--skip-push");

            let previous_head_sha = match flag("--previous-head-sha") {
                Some(v) => v,
                None => {
                    eprintln!("--previous-head-sha is required");
                    std::process::exit(crate::commands::EXIT_USAGE_ERROR);
                }
            };

            let previous_base_sha = flag("--previous-base-sha");

            let head_sha = match flag("--head-sha") {
                Some(v) => v,
                None => {
                    eprintln!("--head-sha is required");
                    std::process::exit(crate::commands::EXIT_USAGE_ERROR);
                }
            };

            let base_sha = flag("--base-sha").unwrap_or_default();

            let base_ref = match flag("--base-ref") {
                Some(v) => v,
                None => {
                    if !base_sha.is_empty() {
                        base_sha.clone()
                    } else {
                        eprintln!("--base-ref is required");
                        std::process::exit(crate::commands::EXIT_USAGE_ERROR);
                    }
                }
            };

            let previous_head_fetch_remote =
                flag("--previous-head-fetch-remote").or_else(|| flag("--remote"));

            let ctx = CiContext {
                repo,
                event: CiEvent::Sync {
                    previous_head_sha,
                    head_sha,
                    base_ref,
                    base_sha,
                    previous_base_sha,
                    previous_head_fetch_remote,
                },
                // Not used for local runs; teardown not invoked
                temp_dir: std::path::PathBuf::from("."),
            };

            tracing::debug!("Local CI context: {:?}", ctx);
            match ctx.run_with_options(CiRunOptions {
                skip_fetch_notes,
                skip_fetch_base: true,
                skip_fetch_fork_notes: false,
                skip_fetch_sync_refs,
                skip_push,
            }) {
                Ok(result) => {
                    tracing::debug!("Local CI result: {:?}", result);
                    print_ci_result(&result, "Local CI (sync)");
                }
                Err(e) => {
                    eprintln!("Error running local CI: {}", e);
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
        other => {
            eprintln!("Unknown local CI event: {}", other);
            print_help_and_exit(CI_LOCAL_HELP, HelpReason::UsageError);
        }
    }
}

const CI_HELP: &str = "\
autter ci - Continuous integration utilities

Usage: autter ci <subcommand> [args...]

Subcommands:
  github           GitHub CI
    run [--no-cleanup]  Run GitHub CI in current repo
    install        Install/update workflow in current repo
  gitlab           GitLab CI
    run [--no-cleanup]  Run GitLab CI in current repo
    install        Print YAML snippet to add to .gitlab-ci.yml
  local            Run CI locally by event name and flags
                   Usage: autter ci local <event> [flags]
                   Events:
                     merge  --merge-commit-sha <sha> --base-ref <ref> --head-ref <ref> --head-sha <sha> --base-sha <sha> [--fork-clone-url <url>]
                            [--skip-fetch-notes] [--skip-fetch-base] [--skip-fetch-fork-notes] [--skip-fetch] [--skip-push]
                     sync   --previous-head-sha <sha> --head-sha <sha> --base-ref <ref> [--base-sha <sha>]
                            [--remote <name-or-url>] [--skip-fetch-notes] [--skip-fetch-sync-refs] [--skip-fetch] [--skip-push]

Add --help to any subcommand for its help. Help never changes anything.
";

const CI_LOCAL_HELP: &str = "\
autter ci local - Run CI locally by event name and flags

Usage: autter ci local <event> [flags]

Events:
  merge  --merge-commit-sha <sha> --base-ref <ref> --head-ref <ref> --head-sha <sha> --base-sha <sha> [--fork-clone-url <url>]
         [--skip-fetch-notes] [--skip-fetch-base] [--skip-fetch-fork-notes] [--skip-fetch] [--skip-push]
  sync   --previous-head-sha <sha> --head-sha <sha> --base-ref <ref> [--base-sha <sha>]
         [--remote <name-or-url>] [--skip-fetch-notes] [--skip-fetch-sync-refs] [--skip-fetch] [--skip-push]
";

const CI_GITHUB_HELP: &str = "\
autter ci github - GitHub CI utilities

Usage: autter ci github <subcommand> [args...]

Subcommands:
  run [--no-cleanup]   Run GitHub CI in current repo
                       --no-cleanup  Skip teardown after run
  install              Install/update workflow in current repo
";

const CI_GITHUB_RUN_HELP: &str = "\
autter ci github run - Run GitHub CI in the current repo

Usage: autter ci github run [--no-cleanup]

  Rewrites authorship notes for the pull request event in the GitHub
  Actions environment, then cleans up.

  --no-cleanup   Skip teardown after the run
";

const CI_GITHUB_INSTALL_HELP: &str = "\
autter ci github install - Install/update the GitHub Actions workflow

Usage: autter ci github install

  Writes .github/workflows/autter.yaml in the current repository,
  overwriting an existing copy.
";

const CI_GITLAB_HELP: &str = "\
autter ci gitlab - GitLab CI utilities

Usage: autter ci gitlab <subcommand> [args...]

Subcommands:
  run [--no-cleanup]   Run GitLab CI in current repo
                       --no-cleanup  Skip teardown after run
  install              Print YAML snippet to add to .gitlab-ci.yml
";

const CI_GITLAB_RUN_HELP: &str = "\
autter ci gitlab run - Run GitLab CI in the current repo

Usage: autter ci gitlab run [--no-cleanup]

  Rewrites authorship notes for the merge request in the GitLab CI
  environment, then cleans up.

  --no-cleanup   Skip teardown after the run
";

const CI_GITLAB_INSTALL_HELP: &str = "\
autter ci gitlab install - Print the GitLab CI snippet

Usage: autter ci gitlab install

  Prints a YAML snippet to add to .gitlab-ci.yml. Writes no files.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn args(flags: &[&str]) -> Vec<String> {
        flags.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn help_is_detected_anywhere_in_subcommand_args() {
        assert!(wants_help(&args(&["install", "--help"])));
        assert!(wants_help(&args(&["install", "-h"])));
        assert!(wants_help(&args(&["--help"])));
        assert!(wants_help(&args(&["help"])));
        assert!(wants_help(&args(&["merge", "--base-ref", "main", "-h"])));
        assert!(!wants_help(&args(&["install"])));
        assert!(!wants_help(&args(&["run", "--no-cleanup"])));
        assert!(!wants_help(&args(&["run", "--helpful"])));
        // `help` is a help request only as the subcommand itself.
        assert!(!wants_help(&args(&["merge", "--base-ref", "help"])));
    }
}
