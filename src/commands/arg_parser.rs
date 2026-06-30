//! Shared global-flag parsing for the **direct** `autter <subcommand>` path.
//!
//! Every direct subcommand handler used to hand-roll its own
//! `while i < args.len()` loop, which is why global flags were inconsistent
//! and undiscoverable (`--json` on some, `--plain` on others, no `-C`, ad-hoc
//! `--help`). This module provides a thin, opt-in pre-parser that handlers call
//! first to extract the standardized global flags
//! (`--json`, `--quiet`, `--no-color`, `-C <path>`, `-h`/`--help`) before doing
//! their own remaining parsing on the leftover args.
//!
//! This is deliberately scoped to the direct path only. The git proxy
//! (`commands::git_handlers`) must keep passing unknown flags straight to git
//! and is never routed through here.

use std::io::IsTerminal;
use std::sync::{OnceLock, RwLock};

/// Standardized global flags recognized on every direct `autter` subcommand.
#[derive(Debug, Clone, Default)]
pub struct GlobalFlags {
    pub json: bool,
    pub quiet: bool,
    pub no_color: bool,
    /// Resolved `-C <path>` value, if supplied. The caller chdirs into it.
    pub change_dir: Option<String>,
    pub help: bool,
}

/// How far into the arg list to scan for global flags.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// Scan the whole arg list. Use for commands whose positionals never start
    /// with `-` (status, stats, file-changes, fetch-notes).
    Full,
    /// Stop scanning at the first non-flag positional or `--`. Use for
    /// forwarding / positional commands (blame, diff, log, checkpoint) so a
    /// downstream git-style flag or ref is never stolen.
    LeadingOnly,
}

/// Result of pre-parsing: extracted globals + the leftover args the subcommand
/// handler does its own parsing on.
pub struct PreParse {
    pub flags: GlobalFlags,
    pub rest: Vec<String>,
}

/// Pre-parse `args`, extracting standardized global flags into [`GlobalFlags`]
/// and returning the remaining args in `rest`.
///
/// * `mode` — see [`ScanMode`].
/// * `recognize_change_dir` — whether `-C <path>` is treated as the global
///   change-directory flag. This must be `false` for forwarding commands like
///   `blame` where `-C` has its own meaning (copy detection); for those, `-C`
///   in a leading position is handled once at the top level in
///   `handle_autter` instead.
///
/// Unknown flags are intentionally **not** errored here — they fall through to
/// `rest` so each subcommand's own parser remains in charge of its grammar and
/// error messages.
pub fn pre_parse(
    args: &[String],
    mode: ScanMode,
    recognize_change_dir: bool,
) -> Result<PreParse, String> {
    let mut flags = GlobalFlags::default();
    let mut rest = Vec::with_capacity(args.len());
    let mut stop = false;
    let mut i = 0;
    while i < args.len() {
        if stop {
            rest.push(args[i].clone());
            i += 1;
            continue;
        }
        let a = args[i].as_str();
        if a == "--" {
            // Everything from `--` onward passes through verbatim.
            rest.extend_from_slice(&args[i..]);
            break;
        }
        match a {
            "-h" | "--help" => {
                flags.help = true;
                i += 1;
            }
            "--json" => {
                flags.json = true;
                i += 1;
            }
            "--quiet" => {
                flags.quiet = true;
                i += 1;
            }
            "--no-color" => {
                flags.no_color = true;
                i += 1;
            }
            "-C" if recognize_change_dir => {
                let p = args
                    .get(i + 1)
                    .ok_or_else(|| "-C requires a <path> argument".to_string())?;
                flags.change_dir = Some(p.clone());
                i += 2;
            }
            _ => {
                // In LeadingOnly mode the first positional ends global scanning
                // so subcommand-specific flags / refs that begin with `-` are
                // preserved verbatim.
                if mode == ScanMode::LeadingOnly && !a.starts_with('-') {
                    stop = true;
                }
                rest.push(args[i].clone());
                i += 1;
            }
        }
    }
    Ok(PreParse { flags, rest })
}

// ===========================================================================
// Process-wide resolved global flags
// ===========================================================================
//
// Color/quiet decisions are read deep inside render code that we don't want to
// thread `GlobalFlags` through. We store the resolved flags in a set-once
// singleton (mirroring the `Config::get()` and `IS_TERMINAL` OnceLock patterns)
// that the top-level dispatch and each migrated handler merge into.

static STORE: OnceLock<RwLock<GlobalFlags>> = OnceLock::new();

fn store() -> &'static RwLock<GlobalFlags> {
    STORE.get_or_init(|| RwLock::new(GlobalFlags::default()))
}

/// Merge any `true`/`Some` fields of `f` into the process-wide global flags.
/// Called once at the top level (for leading globals) and again by each
/// migrated handler (for its own trailing globals).
pub fn merge_global_flags(f: &GlobalFlags) {
    let mut g = store().write().unwrap();
    if f.json {
        g.json = true;
    }
    if f.quiet {
        g.quiet = true;
    }
    if f.no_color {
        g.no_color = true;
    }
}

/// Whether `--json` output was requested anywhere (leading or trailing).
pub fn json() -> bool {
    store().read().unwrap().json
}

/// Whether non-essential human-facing output should be suppressed.
pub fn quiet() -> bool {
    store().read().unwrap().quiet
}

/// Whether coloring is forced off regardless of the output stream: the
/// `--no-color`/`--plain` global flag, or the standard `NO_COLOR` env var.
fn color_forced_off() -> bool {
    store().read().unwrap().no_color || std::env::var_os("NO_COLOR").is_some()
}

/// Centralized color decision for **stdout**: honor `--no-color`/`--plain`, the
/// `NO_COLOR` env var, and whether stdout is a TTY. This is the project-wide
/// `should_colorize()` for human-readable output written via `print!`/`println!`.
pub fn use_color() -> bool {
    !color_forced_off() && std::io::stdout().is_terminal()
}

/// Same decision as [`use_color`] but keyed on **stderr**'s TTY, for color
/// emitted via `eprint!`/`eprintln!` (warnings, notices, progress messages).
pub fn use_color_stderr() -> bool {
    !color_forced_off() && std::io::stderr().is_terminal()
}

/// Wrap `text` in the SGR `code` (e.g. `"1;32"`) when stdout coloring is
/// enabled, otherwise return it unchanged. Keeps call sites readable while
/// routing every color decision through [`use_color`].
pub fn paint(code: &str, text: &str) -> String {
    if use_color() {
        format!("\x1b[{}m{}\x1b[0m", code, text)
    } else {
        text.to_string()
    }
}

/// Like [`paint`] but keyed on stderr (for messages printed via `eprintln!`).
pub fn paint_err(code: &str, text: &str) -> String {
    if use_color_stderr() {
        format!("\x1b[{}m{}\x1b[0m", code, text)
    } else {
        text.to_string()
    }
}

// ===========================================================================
// Help registry
// ===========================================================================

struct HelpEntry {
    name: &'static str,
    aliases: &'static [&'static str],
    summary: &'static str,
    body: &'static str,
}

/// Per-command help. `body` is the detailed text shown by
/// `autter <cmd> --help` / `autter help <cmd>`; `summary` is the one-line shown
/// in the top-level overview. Keep this in sync with the real flags each
/// handler accepts — the overview is generated from `summary` so it can't drift.
static HELP_REGISTRY: &[HelpEntry] = &[
    HelpEntry {
        name: "checkpoint",
        aliases: &[],
        summary: "Checkpoint working changes and attribute author",
        body: "\
autter checkpoint <preset> [--hook-input <json|stdin>] [files...]

  Checkpoint working changes and attribute authorship.

  Presets: claude, codex, continue-cli, cursor, gemini, github-copilot, amp,
           windsurf, opencode, pi, ai_tab, firebender, human, mock_ai,
           mock_known_human, known_human

  --hook-input <json|stdin>   JSON payload required by presets, or 'stdin' to
                              read from stdin
  human [pathspecs...]             Untracked/legacy human checkpoint
  mock_ai [pathspecs...]           Test preset accepting optional file pathspecs
  mock_known_human [pathspecs...]  Test preset for KnownHuman checkpoints",
    },
    HelpEntry {
        name: "log",
        aliases: &[],
        summary: "Show commit log with AI authorship stats",
        body: "\
autter log [args...]

  Show commit log with AI authorship stats. Accepts git log arguments.

  --raw, --notes   Include raw authorship note data
  --plain          Disable colored/decorated output",
    },
    HelpEntry {
        name: "blame",
        aliases: &[],
        summary: "Git blame with AI authorship overlay",
        body: "\
autter blame <file>

  Git blame with an AI authorship overlay. Accepts git blame arguments
  (-L, -C, --porcelain, etc.).

  --json   Output in JSON format",
    },
    HelpEntry {
        name: "diff",
        aliases: &[],
        summary: "Show diff with AI authorship annotations",
        body: "\
autter diff <commit|range>

  Show a diff with AI authorship annotations.

  <commit>              Diff from commit's parent to commit
  <commit1>..<commit2>  Diff between two commits
  --json                Output in JSON format
  --include-stats       Include commit_stats in JSON output (single commit only)
  --all-prompts         Include all prompts from commit note in JSON output
                        (single commit only)",
    },
    HelpEntry {
        name: "stats",
        aliases: &[],
        summary: "Show AI authorship statistics for a commit",
        body: "\
autter stats [commit]

  Show AI authorship statistics for a commit or range.

  --json            Output in JSON format
  --ignore <pat>    Ignore files matching the given pattern(s)",
    },
    HelpEntry {
        name: "file-changes",
        aliases: &[],
        summary: "Show the most frequently changed files in this repo",
        body: "\
autter file-changes [options]

  Show the most frequently changed files in this repository.

  --json            Output in JSON format
  --limit, -n <n>   Number of files to show (default: 20)",
    },
    HelpEntry {
        name: "status",
        aliases: &[],
        summary: "Show uncommitted AI authorship status (debug)",
        body: "\
autter status [--json]

  Show uncommitted AI authorship status since the last commit (debug).

  --json   Output in JSON format",
    },
    HelpEntry {
        name: "show",
        aliases: &[],
        summary: "Display authorship logs for a revision or range",
        body: "\
autter show <rev|range>

  Display authorship logs for a revision or range.",
    },
    HelpEntry {
        name: "show-prompt",
        aliases: &[],
        summary: "Display a prompt record by its ID",
        body: "\
autter show-prompt <id>

  Display a prompt record by its ID.

  --commit <rev>   Look in a specific commit only
  --offset <n>     Skip n occurrences (0 = most recent, mutually exclusive
                   with --commit)",
    },
    HelpEntry {
        name: "config",
        aliases: &[],
        summary: "View and manage autter configuration",
        body: "\
autter config [<key> | set <key> <value> | unset <key>]

  View and manage autter configuration. With no arguments, shows all config as
  formatted JSON.

  <key>                 Show specific config value (supports dot notation)
  set <key> <value>     Set a config value (arrays: single value = [value])
  --add <key> <value>   Add to array or upsert into object
  unset <key>           Remove config value (reverts to default)",
    },
    HelpEntry {
        name: "debug",
        aliases: &[],
        summary: "Print support/debug diagnostics",
        body: "autter debug\n\n  Print support/debug diagnostics.",
    },
    HelpEntry {
        name: "bg",
        aliases: &["d", "daemon"],
        summary: "Run and control autter background service",
        body: "autter bg\n\n  Run and control the autter background service.",
    },
    HelpEntry {
        name: "install-hooks",
        aliases: &["install"],
        summary: "Install git hooks for AI authorship tracking",
        body: "\
autter install-hooks [options]

  Install git hooks for AI authorship tracking.

  --skills                    Also install agent skill files
  --visual-studio-extension   Also install the Visual Studio extension (Windows)",
    },
    HelpEntry {
        name: "uninstall-hooks",
        aliases: &[],
        summary: "Remove autter hooks from all detected tools",
        body: "autter uninstall-hooks\n\n  Remove autter hooks from all detected tools.",
    },
    HelpEntry {
        name: "ci",
        aliases: &[],
        summary: "Continuous integration utilities",
        body: "\
autter ci <subcommand>

  Continuous integration utilities.

  github   GitHub CI helpers",
    },
    HelpEntry {
        name: "squash-authorship",
        aliases: &[],
        summary: "Generate authorship log for squashed commits",
        body: "\
autter squash-authorship <base_branch> <new_sha> <old_sha>

  Generate an authorship log for squashed commits.

  --dry-run   Show what would be done without making changes",
    },
    HelpEntry {
        name: "git-path",
        aliases: &[],
        summary: "Print the path to the underlying git executable",
        body: "autter git-path\n\n  Print the path to the underlying git executable.",
    },
    HelpEntry {
        name: "upgrade",
        aliases: &[],
        summary: "Check for updates and install if available",
        body: "\
autter upgrade [--force]

  Check for updates and install if available.

  --force   Reinstall latest version even if already up to date",
    },
    HelpEntry {
        name: "fetch-notes",
        aliases: &[],
        summary: "Synchronously fetch AI authorship notes",
        body: "\
autter fetch-notes [options] [<remote>]

  Synchronously fetch AI authorship notes from a remote.

  <remote>          Remote to fetch from (default: upstream or origin)
  --remote <name>   Explicit remote name
  --json            Output result as JSON

Examples:
  autter fetch-notes             Fetch from default remote
  autter fetch-notes upstream    Fetch from 'upstream' remote
  autter fetch-notes --json      Fetch and output JSON result",
    },
    HelpEntry {
        name: "onboard",
        aliases: &["onboarding", "setup"],
        summary: "Set up Autter (connect to the platform or run local)",
        body: "\
autter onboard [options]

  Set up Autter (connect to the platform or run local).

  --connect   Connect to the Autter platform (runs login)
  --local     Use local-only mode (no uploads)
  --force     Re-run onboarding even if already completed",
    },
    HelpEntry {
        name: "login",
        aliases: &[],
        summary: "Open the dashboard to create a sign-in token",
        body: "\
autter login [--token <token>]

  Open the dashboard to create a sign-in token.

  --token <token>   Complete sign-in with a token from the dashboard",
    },
    HelpEntry {
        name: "logout",
        aliases: &[],
        summary: "Clear stored credentials",
        body: "autter logout\n\n  Clear stored credentials.",
    },
    HelpEntry {
        name: "whoami",
        aliases: &[],
        summary: "Show auth state and login identity",
        body: "autter whoami\n\n  Show auth state and login identity.",
    },
    HelpEntry {
        name: "telemetry",
        aliases: &[],
        summary: "Inspect or change anonymous telemetry",
        body: "\
autter telemetry <subcommand>

  Inspect or change anonymous telemetry.

  status            Show on/off state and the local audit log path
  log [-n N|--all]  Print the local audit log of data sent
  on | off          Enable or disable telemetry",
    },
];

/// Print the detailed help for a single subcommand, falling back to the
/// top-level overview for unknown commands.
pub fn print_command_help(cmd: &str) {
    if let Some(e) = HELP_REGISTRY
        .iter()
        .find(|e| e.name == cmd || e.aliases.contains(&cmd))
    {
        eprintln!("{}", e.body);
    } else {
        print_overview();
    }
}

/// Print the top-level overview, generating the command list from the registry.
pub fn print_overview() {
    eprintln!("autter - git proxy with AI authorship tracking");
    eprintln!();
    eprintln!("Usage: autter [-C <path>] <command> [args...]");
    eprintln!();
    eprintln!("Global flags (available on most commands):");
    eprintln!("  --json        Output in JSON format");
    eprintln!("  --quiet       Suppress non-essential output");
    eprintln!("  --no-color    Disable colored output");
    eprintln!("  -C <path>     Run as if autter was started in <path>");
    eprintln!("  -h, --help    Show help (use 'autter <command> --help' for details)");
    eprintln!();
    eprintln!("Commands:");
    for e in HELP_REGISTRY {
        eprintln!("  {:<18} {}", e.name, e.summary);
    }
    eprintln!("  {:<18} {}", "version, -v", "Print the autter version");
    eprintln!("  {:<18} {}", "help, -h", "Show this help message");
    eprintln!();
    eprintln!("Run 'autter <command> --help' for command-specific help.");
}
