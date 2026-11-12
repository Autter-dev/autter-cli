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
    /// `--strict`: `stats` and `status` exit 3 when Autter has no record of
    /// what was added, and `doctor` exits 4 when capture is broken (see
    /// `commands::output`).
    pub strict: bool,
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
            "--strict" => {
                flags.strict = true;
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
    if f.strict {
        g.strict = true;
    }
}

/// Record `--json` when it appears anywhere in `args` before a `--`
/// separator. Called before a command's own parsing (and before the
/// dispatcher contacts the background service), so a failure that happens
/// first is still reported as a JSON document.
pub fn merge_json_flag_from(args: &[String]) {
    if args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--json")
    {
        merge_global_flags(&GlobalFlags {
            json: true,
            ..Default::default()
        });
    }
}

/// Whether `--strict` was given anywhere (leading or trailing). Use
/// [`crate::commands::output::strict`], which also reads `AUTTER_STRICT`.
pub fn strict_flag() -> bool {
    store().read().unwrap().strict
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
/// `--no-color`/`--plain` global flag, the standard `NO_COLOR` env var, `--json`
/// output, or CI (`CI=true`).
fn color_forced_off() -> bool {
    let flags = store().read().unwrap();
    flags.no_color
        || flags.json
        || std::env::var_os("NO_COLOR").is_some()
        || crate::commands::output::ci_mode()
}

/// On Windows, enable VT processing (and report whether ANSI works). Without
/// this, ConHost prints raw `\x1b[…` bytes even when stdout is a TTY. Elsewhere
/// a TTY is enough.
fn terminal_supports_ansi() -> bool {
    #[cfg(windows)]
    {
        crossterm::ansi_support::supports_ansi()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Centralized color decision for **stdout**: honor `--no-color`/`--plain`, the
/// `NO_COLOR` env var, and whether stdout is a TTY. This is the project-wide
/// `should_colorize()` for human-readable output written via `print!`/`println!`.
pub fn use_color() -> bool {
    if color_forced_off() || !std::io::stdout().is_terminal() {
        return false;
    }
    terminal_supports_ansi()
}

/// Same decision as [`use_color`] but keyed on **stderr**'s TTY, for color
/// emitted via `eprint!`/`eprintln!` (warnings, notices, progress messages).
pub fn use_color_stderr() -> bool {
    if color_forced_off() || !std::io::stderr().is_terminal() {
        return false;
    }
    terminal_supports_ansi()
}

/// Whether OSC 8 hyperlinks are safe to emit. Color/VT support is not enough —
/// classic ConHost with VT enabled still dumps OSC 8 sequences as literal text.
pub fn use_hyperlinks() -> bool {
    if !use_color() {
        return false;
    }
    // Explicit opt-out / opt-in (https://gist.github.com/egmontkob/eb114476d1dda0276486)
    if let Ok(v) = std::env::var("FORCE_HYPERLINK") {
        return v != "0";
    }
    if std::env::var_os("DOMTERM").is_some() {
        return true;
    }
    if let Ok(program) = std::env::var("TERM_PROGRAM") {
        match program.as_str() {
            "iTerm.app" | "WezTerm" | "ghostty" | "vscode" | "Hyper" | "kitty" => return true,
            _ => {}
        }
    }
    if let Ok(term) = std::env::var("TERM")
        && (term.contains("kitty") || term.contains("vte") || term == "xterm-kitty")
    {
        return true;
    }
    // Windows Terminal sets WT_SESSION; classic cmd/ConHost does not.
    if std::env::var_os("WT_SESSION").is_some() {
        return true;
    }
    // VTE-based terminals (GNOME Terminal, Tilix, …) advertise via VTE_VERSION ≥ 5000.
    if let Ok(vte) = std::env::var("VTE_VERSION")
        && let Ok(n) = vte.parse::<u32>()
        && n >= 5000
    {
        return true;
    }
    // Non-Windows TTYs often support OSC 8 in modern emulators; keep the prior
    // interactive behavior there. On Windows, require an explicit signal above.
    #[cfg(not(windows))]
    {
        true
    }
    #[cfg(windows)]
    {
        false
    }
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
  --plain          Disable colored/decorated output
  --json           One JSON document: every commit with its authorship stats",
    },
    HelpEntry {
        name: "blame",
        aliases: &[],
        summary: "Git blame with AI authorship overlay",
        body: "\
autter blame <file>

  Git blame with an AI authorship overlay. Accepts git blame arguments
  (-L, -C, --porcelain, etc.).

  On a terminal each line starts with its category: AI, human, untracked
  (no agent or Autter editor extension recorded it) or unknown (its commit
  has no Autter record). Human, untracked and unknown lines show the git
  author; AI lines show the agent.

  On a terminal, the output ends with a legend of the prompts behind its AI
  lines: a short reference (s_3f9a2c1d), the agent and model, and the lines.
  Pass that reference to `autter show-prompt` for the full record.

  --why <file>:<line>   Explain one line: agent, model, prompt reference,
                        session, commit and transcript
  --open                With --why: open the line's issue, PR or prompt URL
  --show-prompt         Show the prompt reference on every AI line
  --json                Output in JSON format (with schema_version)
  --markers             Show the category column even when piped
  --no-markers          Hide it on a terminal (plain git blame layout)
  -i, --interactive     Browse the file full-screen: AI, human and untracked
                        lines, and the prompt behind each AI change",
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
  --json                Output in JSON format (with schema_version)
  --include-stats       Include commit_stats in JSON output (single commit only)
  --all-prompts         Include all prompts from commit note in JSON output
                        (single commit only)",
    },
    HelpEntry {
        name: "stats",
        aliases: &[],
        summary: "Show AI authorship statistics for a commit",
        body: "\
autter stats [commit|<a>..<b>]

  Show AI authorship statistics for a commit or range.

  Lines are counted as AI (an agent wrote them), human (typed in an editor
  with the Autter extension), untracked (Autter recorded the commit but
  nothing recorded the line, usually typed by hand) or unknown (the commit
  has no Autter record). AI % is the share of added lines an agent wrote
  into the file, not who directed or reviewed the work.

  Right after a commit, stats waits up to 10s for the background service to
  write the commit's record.

  --json              One JSON document: lines (added, deleted, ai, human,
                      untracked, unknown), percent, capture.status
                      (captured | nothing_captured | no_additions), and
                      `record`: whether the commit has an Autter record and,
                      if not, why
  --strict            Exit 3 when lines were added and Autter has no record
                      of them (every added line is unknown): capture missed
                      the commit. A recorded commit typed entirely by hand
                      exits 0. A merge commit is judged on the lines it adds
                      itself (also AUTTER_STRICT=1)
  --ignore <pat>      Ignore files matching the given pattern(s)
  --wait[=secs]       Wait for the authorship note (default 30s); exit 1 if still pending
  --coverage [range]  Which commits have an Autter record (default: the last
                      50 from HEAD), why the others don't, and whether the
                      records are on the remote
  -i, --interactive   Browse recent commits full-screen by AI share, with each
                      commit's breakdown by tool and model",
    },
    HelpEntry {
        name: "badge",
        aliases: &[],
        summary: "README badge with the repo's AI-written share",
        body: "\
autter badge [--since <date|duration> | --all-time] [--json] [--write [path]]

  Print the share of this repository's added lines that were written by AI
  (AI-accepted lines / added lines, over commits with an authorship note) and
  a README badge snippet reading `AI-written | 72%`. The snippet is copied to
  the clipboard when stderr is a terminal.

  By default the last 90 days of non-merge commits on the current branch are
  counted. Commits without an authorship note add nothing to the share but
  are counted, so the output reports coverage. At most 2000 commits are
  walked. Files matching .autter-ignore are left out.

  --since <value>     Count commits since a date (2025-08-05) or duration
                      (30 days ago); anything `git log --since` accepts
  --all-time          Count every commit reachable from HEAD
  --ignore <pat>      Also ignore files matching the given pattern(s)
  --json              Print the shields.io endpoint payload: schemaVersion,
                      label, message and color, plus ai_percent, ai_accepted,
                      added_lines, commits, commits_with_authorship, since
                      and capped
  --write [path]      Also write that payload to a file (default
                      .autter/badge.json) that shields.io can read once it is
                      committed: https://img.shields.io/endpoint?url=<raw url>

  Badge colors: no data lightgrey; under 25% light blue (74c0fc);
  25-49% blue (007ec6); 50-74% indigo (5b5bd6); 75% and up teal (2ee6c5).

  When the repository is linked to Autter and you are signed in, the snippet
  uses a live badge served by Autter; otherwise it is a static shields.io
  badge with the local number.",
    },
    HelpEntry {
        name: "wrapped",
        aliases: &[],
        summary: "Your year of AI-assisted coding in this repo, as a share card",
        body: "\
autter wrapped [options]

  A Spotify-Wrapped-style summary of the current user's AI-assisted coding
  in this repository for one year: AI share overall and by month, the top
  agent and model, the most-prompted file, the longest prompt, the longest
  streak and the busiest weekday. Printed as a fixed-width card made for
  screenshots. Nothing leaves the machine.

  --year <yyyy>           Year to summarize (default: the current year)
  --author <email>        Commits by this author (default: git user.email)
  --all-authors           Every author in the repository
  --include-prompt-text   Include the first 120 characters of the longest
                          prompt; off by default so prompt text stays private
  --json                  Output the summary as JSON (the share-card contract)
  --no-color              Plain text, no ANSI colors

  --share                 Publish the summary as a page at app.autter.dev/wrapped/<token>
                          with a share image, and print its link. Sharing again for
                          the same year and repository updates the same page.
  --hide-repo             With --share: show no repository name on the page
  --unshare <url|token>   Take a share page down

  Sharing needs a connected account (autter login); nothing is sent in local mode.
  See app.autter.dev/wrapped/demo for a sample page.",
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
autter status [--json] [--strict]

  Show uncommitted AI authorship status since the last commit (debug).

  --json     One JSON document with lines, percent, capture and checkpoints
  --strict   Exit 3 when there are uncommitted changes, no checkpoint was
             recorded for them and the background service is not running, so
             a commit made now would get no record. Changes typed by hand
             while capture is working exit 0 (also AUTTER_STRICT=1)",
    },
    HelpEntry {
        name: "show",
        aliases: &[],
        summary: "Display authorship logs for a revision or range",
        body: "\
autter show <rev|range> [--json]

  Display authorship logs for a revision or range.

  --json   One JSON document: per commit, note_present and the parsed
           attestations and metadata (null when there is no note)",
    },
    HelpEntry {
        name: "show-prompt",
        aliases: &[],
        summary: "Show the prompt and session behind AI lines, by prompt ID",
        body: "\
autter show-prompt <id> [--commit <rev> | --offset <n>]

  Print, as JSON, the prompt record behind AI-written lines: the agent, the
  model, the agent's own session ID, the human who ran it, line counts, the
  commit whose authorship note holds it, and the transcript when one can be
  read. When there is no transcript, `transcript.reason` says why.

  Where to find the ID:
    autter blame <file>               on a terminal, blame ends with a legend
                                      of the prompts behind its AI lines
    autter blame --show-prompt <file> the same reference on every AI line
    autter blame --why <file>:<line>  the prompt behind one line
    autter show <commit>              the raw note (prompts and sessions)
    autter log --notes                the raw notes in history

  The ID can be:
    s_3f9a2c1d                 a short reference from blame (any unique
                               prefix of 4+ hex characters works)
    s_3f9a2c1d4e5f60           a full session ID
    s_3f9a2c1d4e5f60::t_...    a line's attestation key (same session)
    0123456789abcdef           a prompt hash from older notes
    <agent session id>         the agent's own ID, e.g. a Claude Code session

  From a line to its change: blame shows the commit and the prompt
  reference; --why and show-prompt add the agent, model, session and
  transcript; `git show <commit>` is the change itself.

  Transcripts are read from this machine first, then from Autter when you
  are connected and signed in. Secrets are redacted before a transcript is
  stored. Notes made on other machines arrive with `autter fetch-notes`.

  --commit <rev>   Look in that commit's note only
  --offset <n>     Skip n occurrences, newest first (0 = most recent);
                   cannot be combined with --commit. Commits made in the
                   same second are ordered by history (a commit before its
                   ancestors); unrelated ones by ascending commit SHA",
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
        name: "doctor",
        aliases: &[],
        summary: "Validate the autter setup end-to-end",
        body: "autter doctor [--json] [--strict] [--skip-trace2-checks] [--report <file>]

  Run setup validation checks: git and config, the background service,
  trace2 capture, a real end-to-end checkpoint round-trip, AI agent hooks,
  and login/connectivity. Every failed check prints a concrete fix.

  Exits 0 when no check fails (warnings allowed), 1 when any check fails.
  With --strict, exits 4 instead when a check capture depends on fails
  (background service, trace2, end-to-end checkpoint, agent hooks).

  --json                 Machine-readable single-line JSON report
                         (capture_ok, and an error object when it fails)
  --strict               Exit 4 when capture is broken (also AUTTER_STRICT=1)
  --skip-trace2-checks   Skip the trace2 event-capture self-check
  --report <file>        Also write a support report to <file>: these
                         results, service start timings, the lock file,
                         the last 200 lines of the service log, git and OS
                         versions, and whether this runs elevated. No
                         source code, file contents or tokens.",
    },
    HelpEntry {
        name: "sync",
        aliases: &[],
        summary: "Inspect cloud upload health and local queue depth",
        body: "autter sync status [--json]
autter sync purge --force
autter sync import --yes
autter sync retry
autter sync open

  Show local upload queues, the last confirmed metrics batch, and the next
  action for a blocked upload. Open the dashboard for the reported organization.

  `purge` discards the local upload backlog without uploading it — use before
  login if you do not want historical sessions sent to the cloud.

  `import` uploads the held backlog: records queued before you connected this
  machine, or by an older autter version. Held records never upload on their
  own. Without --yes it only shows what is held.

  `retry` queues usage-metrics events again that were set aside because the
  server failed on them every time. Set-aside events are kept, not uploaded.

  An empty queue does not confirm upload of every local change.
  Status exits 0 when upload is off or no problem is detected. It exits 1
  when sign-in, the background service, an upload, or a queue read needs attention.

  --json   Machine-readable JSON report",
    },
    HelpEntry {
        name: "dashboard",
        aliases: &["dash", "personal-dashboard", "personal_dashboard"],
        summary: "Open your personal dashboard in the browser",
        body: "\
autter dashboard

  Open your personal dashboard in the browser
  (https://app.autter.dev/me, or your configured web app URL).",
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

  Install IDE/agent hooks for AI authorship tracking.

  --system                    Also write global git trace2 config and start the daemon
                              (preferred via `autter onboard`, which asks for consent)
  --skills                    Also install agent skill files
  --visual-studio-extension   Also install the Visual Studio extension (Windows)
  --dry-run                   Show what would change without changing anything
  -v, --verbose               Show the diff of every file that changes",
    },
    HelpEntry {
        name: "uninstall-hooks",
        aliases: &[],
        summary: "Remove autter hooks from all detected tools",
        body: "\
autter uninstall-hooks [options]

  Remove autter hooks, and the editor extensions autter installed, from all
  detected tools. Global git trace2 config and the background service are left
  alone; use `autter uninstall` for those too.

  --dry-run       Show what would be removed without removing anything
  -v, --verbose   Show the diff of every file that changes",
    },
    HelpEntry {
        name: "uninstall",
        aliases: &[],
        summary: "Remove hooks, git trace2 config, and stop the background service",
        body: "\
autter uninstall [--yes] [--dry-run] [--verbose]

  Remove IDE/agent hooks and the editor extensions autter installed, clear
  global git trace2 settings, and stop the background service. Does not delete
  the binary or PATH entries.

  Asks for confirmation at a terminal. Without a terminal (scripts, CI) it
  refuses unless --yes is given.

  -y, --yes       Don't ask for confirmation
  --dry-run       Show what would be removed without removing anything
  -v, --verbose   Show the diff of every file that changes",
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

  --connect         Connect to the Autter platform (runs login)
  --local           Use local-only mode (no uploads)
  --force           Re-run onboarding even if already completed
  --yes, -y         Install system integrations (IDE/agent hooks, global git
                    trace2 settings, background service) without asking. With
                    --connect, also upload data queued before connecting
  --telemetry       Enable anonymous telemetry without asking
  --no-telemetry    Disable anonymous telemetry without asking

  Choosing a mode never changes system configuration on its own. Without
  --yes, onboarding asks first in a terminal and skips the integrations in
  non-interactive runs (install them later with `autter install --system`).
  Data queued before connecting is held until you agree to upload it
  (`autter sync import --yes`) or delete it (`autter sync purge --force`).",
    },
    HelpEntry {
        name: "login",
        aliases: &[],
        summary: "Sign in to Autter in the browser",
        body: "\
autter login [--token <token>]

  Sign in to Autter. Opens a browser, shows a one-time code, and waits
  until you approve the device. No token paste is required.

  --token <token>   Non-interactive fallback: complete sign-in with a
                    Personal Access Token from Settings -> Access Tokens",
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
        body: "autter whoami [--json]\n\n  Show auth state and login identity.\n\n  --json   One JSON document; exits 1 (not_signed_in) when signed out.",
    },
    HelpEntry {
        name: "ask",
        aliases: &[],
        summary: "Investigate your system with Runtime evidence",
        body: "autter ask <question> [--repo <name>] [--org <slug>] [--thread <id>] [--mode system|codebase|reviews] [--json]\n\n  Get root causes, impact, and recommended fixes. Questions and answers are\n  saved in the same private thread history as Dashboard Ask. Use --thread to resume.\n  Sign in with autter login. Default mode: system.",
    },
    HelpEntry {
        name: "logs",
        aliases: &[],
        summary: "Query captured Runtime logs in a readable table",
        body: "autter logs [--repo <name>] [--org <slug>] [--severity <level>] [--service <name>]\n            [--environment <name>] [--since 1h|7d] [--from <ISO date>] [--to <ISO date>]\n            [--query <text>] [--field field=value] [--filter field:operator:value]\n            [--limit 1..200] [--json]\n\n  Search captured errors, warnings and info (default: last 24h). Custom fields\n  use attributes.<name>. Repeat --field or --filter to combine filters with AND.\n  Operators: eq, neq, contains, gt, gte, lt, lte, exists. Use --json for all fields.",
    },
    HelpEntry {
        name: "threads",
        aliases: &[],
        summary: "Search and read saved debugging investigations",
        body: "autter threads [search <keywords>] [--repo <name>] [--org <slug>] [--offset <n>] [--json]\nautter threads show <id> [--org <slug>] [--json]\n\n  Search your questions and answers across Dashboard and CLI. Lists 50 threads\n  at a time. Continue an investigation with autter ask <question> --thread <id>.",
    },
    HelpEntry {
        name: "memory",
        aliases: &[],
        summary: "Search this repository's memory in the Autter cloud",
        body: "autter memory search <question> [--path <file|dir>]... [--kind <kind>]... [--limit 1..30]
                     [--repo <remote|owner/repo>] [--org <slug>] [--json]
autter memory for [<file>...] [--repo <remote|owner/repo>] [--org <slug>] [--json]
autter memory show [<page>] [--repo <remote|owner/repo>] [--org <slug>] [--json]
autter memory areas | ledger [--repo <remote|owner/repo>] [--org <slug>] [--json]
autter memory skills install [--force] [--print] [--repo <remote|owner/repo>] [--org <slug>]
autter memory skills preview [--repo <remote|owner/repo>] [--org <slug>] [--json]

  for     Print the area pages of the memory covering these files (default: the
          files changed in this checkout), most specific first: how that part of
          the codebase works, its conventions, what breaks and what works there,
          then the what-works ledger lines about those areas.
  search  Rank what Autter knows about this repository by meaning: its shared
          memory pages, learnings, production lessons, team rules, past review
          and scan findings, the wiki and code. --path keeps results about those
          files plus repo-wide knowledge. --kind narrows the sources (repeat or
          comma-separate): repo_memory, learnings, lessons, rules, memory,
          repo_context, review_findings, scan_findings, docs, incidents,
          pr_summaries, tickets, code, symbols.
  show    Print the repository memory home page, its areas and pages, or one page.
  areas   List the area pages and the part of the codebase each covers.
  ledger  Print what worked, what didn't, and what the team waves off.
  skills  install writes the repository's memory skill from Autter into this
          checkout (.agents/skills/autter-repo-memory/SKILL.md and a Cursor
          rule) so teammates' agents get it from git once committed. It
          replaces only files Autter wrote (autter_generated: true in their
          frontmatter); --force replaces others too. preview (or install
          --print) prints the files and writes nothing.

  For one bounded answer for a task (facts, wiki and code), use
  `autter context \"<task>\"`.

  The repository is this checkout's default remote unless --repo is given.
  Uses your autter login. Nothing is sent in local mode or for repositories
  excluded from sharing. Use --json in scripts and coding agents.",
    },
    HelpEntry {
        name: "context",
        aliases: &[],
        summary: "What Autter knows that matters for a task, for coding agents",
        body: "autter context <task> [--path <file>]... [--symbol <name>]... [--budget 300..16000]
               [--include memory,wiki,code] [--since <cursor>]
               [--repo <remote|owner/repo>] [--org <slug>] [--json]

  One call that returns, for the task, the repository memory facts, wiki pages
  and code structure that matter, ranked and cut to a token budget, as Markdown
  for an agent to read: one line per item with its source, title, path, the
  fact id to cite (rmf_...), and where to read more. The same as the get_context
  MCP tool, for agents not connected to the Autter MCP server.

  --path      A file the task is about (repeatable; default: the files changed
              in this checkout). Relative to where you run the command.
  --symbol    A function, class or type the task is about (repeatable).
  --budget    Tokens of context to return (default: the server's, 300 to 16000).
  --include   Only these stores, comma-separated: memory, wiki, code.
  --since     The cursor a previous call printed: leaves out what it already
              returned unchanged.
  --json      The raw response.

  The repository is this checkout's default remote unless --repo is given.
  Uses your autter login. Nothing is sent in local mode or for repositories
  excluded from sharing.",
    },
    HelpEntry {
        name: "mcp",
        aliases: &[],
        summary: "Connect your coding agents to Autter's MCP server",
        body: "autter mcp connect [--agent <name>]... [--dry-run] [--json]
autter mcp disconnect [--agent <name>]... [--dry-run] [--json]

  connect     Add Autter's MCP server (<api base>/mcp) as `autter` to the
              user-level MCP config of each installed agent: Claude Code
              (claude mcp add, else ~/.claude.json), Codex (codex mcp add, else
              ~/.codex/config.toml), Cursor (~/.cursor/mcp.json), VS Code (the
              user mcp.json), Gemini CLI (~/.gemini/settings.json) and OpenCode
              (~/.config/opencode/opencode.json). Only the URL is written: the
              server signs you in with OAuth the first time an agent uses it.
              An existing `autter` entry is replaced, never duplicated.
  disconnect  Remove only the `autter` entry from each agent.

  --agent     Only this agent (repeatable or comma-separated): claude, codex,
              cursor, vscode, gemini, opencode.
  --dry-run   Show the commands and file changes without making them.

  Agents that are not installed are skipped. Every file is backed up next to
  itself (<file>.autter-backup-<time>) before it is changed.",
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

/// True when `args` (a subcommand's arguments) ask for help: `-h` or `--help`
/// in any position before a `--` separator. Exact matches only, so a value such
/// as a commit message that merely mentions `--help` doesn't count.
pub fn wants_help(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "-h" || a == "--help")
}

/// Subcommands whose own handler prints richer (often per-subcommand) help and
/// checks for `-h`/`--help` in every position before doing any work. Every
/// other subcommand gets its help from the dispatcher, which never calls the
/// handler at all. A command is only listed here once its handler is known to
/// return on help before any side effect: list it and it can no longer be
/// protected by the dispatcher.
const HANDLES_OWN_HELP: &[&str] = &[
    "ask",
    "logs",
    "threads",
    "memory",
    "context",
    "mcp",
    "ci",
    "debug",
    "onboard",
    "onboarding",
    "setup",
    "login",
    "whoami",
    "notes",
    "sync",
    "telemetry",
    "config",
    "bg",
    "d",
    "daemon",
];

/// Whether the dispatcher should leave `-h`/`--help` to the subcommand's own
/// handler (see [`HANDLES_OWN_HELP`]).
pub fn handles_own_help(cmd: &str) -> bool {
    HANDLES_OWN_HELP.contains(&cmd)
}

/// The error a subcommand prints for an argument it doesn't accept.
pub fn unknown_option_message(cmd: &str, arg: &str) -> String {
    format!(
        "error: unknown option '{arg}' for 'autter {cmd}'\nRun 'autter {cmd} --help' for usage."
    )
}

/// Print [`unknown_option_message`] and exit with the usage-error code. Used by
/// subcommands that change the machine, so a mistyped flag never runs them
/// with defaults the user didn't ask for.
pub fn exit_unknown_option(cmd: &str, arg: &str) -> ! {
    crate::commands::output::fail(
        crate::commands::output::ErrorKind::Usage,
        unknown_option_message(cmd, arg),
    )
}

/// Exit with a usage error when `args` holds anything at all: for subcommands
/// that take no arguments.
pub fn reject_any_args(cmd: &str, args: &[String]) {
    if let Some(arg) = args.first() {
        exit_unknown_option(cmd, arg);
    }
}

/// Every command name and alias in the help registry.
pub fn documented_commands() -> impl Iterator<Item = &'static str> {
    HELP_REGISTRY
        .iter()
        .flat_map(|e| std::iter::once(e.name).chain(e.aliases.iter().copied()))
}

/// Whether `cmd` (or one of its aliases) has an entry in the help registry.
pub fn has_command_help(cmd: &str) -> bool {
    HELP_REGISTRY
        .iter()
        .any(|e| e.name == cmd || e.aliases.contains(&cmd))
}

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
    eprintln!("  --json        One JSON document on stdout, with schema_version");
    eprintln!("  --strict      stats/status: exit 3 when nothing was captured (Autter has");
    eprintln!("                no record of the added lines); doctor: exit 4 when");
    eprintln!("                capture is broken (also AUTTER_STRICT=1)");
    eprintln!("  --quiet       Suppress non-essential output");
    eprintln!("  --no-color    Disable colored output");
    eprintln!("  -C <path>     Run as if autter was started in <path>");
    eprintln!("  -h, --help    Show help (use 'autter <command> --help' for details)");
    eprintln!();
    eprintln!("Commands:");
    for e in HELP_REGISTRY {
        eprintln!("  {:<18} {}", e.name, e.summary);
    }
    eprintln!("  {:<18} Print the autter version", "version, -v");
    eprintln!("  {:<18} Show this help message", "help, -h");
    eprintln!();
    eprint!("{EXIT_CODES_HELP}");
    eprintln!();
    eprintln!("Run 'autter <command> --help' for command-specific help.");
}

/// The exit-code table printed by `autter --help`. Keep in sync with
/// `commands::output` and the README.
pub const EXIT_CODES_HELP: &str = "Exit codes:
  0   Success
  1   Runtime error
  2   Usage error (bad flags, outside a repository)
  3   Nothing captured: Autter has no record of the added lines
      (stats, status; only with --strict)
  4   Capture is broken (doctor; only with --strict)

With --json, a failure prints {\"schema_version\":1,\"error\":{\"code\",\"message\",\"exit_code\"}}.
In CI (CI=true) or when output is not a terminal: no color, prompts or spinners.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_help_resolves_all_invocation_names() {
        // `autter personal-dashboard` must not fall through to
        // "Unknown autter command" (dispatch in autter_handlers.rs routes
        // these same names to handle_personal_dashboard).
        for name in [
            "dashboard",
            "dash",
            "personal-dashboard",
            "personal_dashboard",
        ] {
            assert!(
                HELP_REGISTRY
                    .iter()
                    .any(|e| e.name == name || e.aliases.contains(&name)),
                "help registry should resolve `{name}`"
            );
        }
    }
}
