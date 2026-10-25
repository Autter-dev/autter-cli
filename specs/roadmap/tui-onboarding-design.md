# TUI redesign: signup and everyday usage

Status: design approved, open questions decided (§7). Nothing implemented yet.

This plan covers the first-run path (install → `autter onboard` → sign-in → integrations) and the commands people use afterwards (`whoami`, `doctor`, `install-hooks`, the upgrade prompt). It describes how things work today, the design rules, which TUI package to use, the target screens, and the order to build them in.

---

## 1. How it works today

### The signup path

```
install.sh / install.ps1
  └─ autter onboard  (</dev/tty under curl | sh)
       1. print_welcome                      onboard.rs:429
       2. Mode select (Connected / Local)    onboard.rs:255   ui::select
       3. Device login (if connecting)       login.rs:16      eprintln + indicatif spinner
       4. Telemetry consent                  onboard.rs:272   6 lines of text + ui::select
       5. System integrations                onboard.rs:571   ui::select, then install_hooks output
       6. Backlog import/discard (if any)    onboard.rs:674   ui::select
       7. render_finished summary            onboard.rs:445
  └─ installer prints its PATH notice
```

### Problems

**Visual consistency**
- Output uses three different styles:
  - `src/ui.rs` has a hand-written selector (`❯` cursor, collapses to `✓ question · answer`) on stderr.
  - `src/mdm/spinner.rs` uses an indicatif spinner, but its result lines are `println!`, so they go to **stdout** (`spinner.rs:47-66`).
  - `login.rs:65,68,105` writes raw `\x1b[...]` escapes, which skips the `NO_COLOR` / `--no-color` handling in `arg_parser::paint`.
- `install_hooks` prints a bold "Coding Agents" header and one spinner per agent to stdout, in the middle of an otherwise stderr-only onboarding flow.
- Glyphs (`✓ ❯ ⚠ 🦦 ⠋`) have no ASCII fallback for legacy Windows consoles or for fonts that lack them.

**Copy and pacing**
- The welcome says "two quick choices" (`onboard.rs:437`), but a user can get up to four prompts: mode, telemetry, integrations and backlog.
- The telemetry step opens with "One last thing" (`onboard.rs:279`), and two more steps follow it.
- Nothing shows the user where they are in the flow, such as "step 2 of 4".
- The telemetry step shows a 6-line dim paragraph before the question. The question is the part people need to read, and it is easy to miss.

**Sign-in**
- The URL and code are plain lines. The code is not copied anywhere, Enter does not reopen the browser, and nothing shows when the code expires.
- If the browser does not open, the only notice is a quiet `(Could not open browser automatically)`.
- The spinner text stays "Waiting for you to authorize…" for the whole wait.
- When sign-in fails, the flow prints the error, then a 6-line PAT walkthrough, then a green "✓ Autter is set up in local mode" block (`onboard.rs:388-399`). The user sees a failure and a success at the same moment and is never asked what they want to do next.

**After setup**
- `whoami` prints flat `Key: value` lines on stdout.
- `doctor` prints sections only after every check has finished, with no live progress.
- Running bare `autter` prints the full command overview, not anything about the user's own setup.

**Dependencies**
- `ratatui = "0.30"` is in `Cargo.toml`, but nothing in `src/` or `tests/` uses it. It adds compile time for no benefit.

### Behavior the redesign must keep

These rules already work and must not break:
- Prompts and decoration go to stderr. Stdout stays scriptable.
- Every prompt has a non-interactive default and a flag. Examples: `--connect/--local`, `--telemetry/--no-telemetry`, `--yes`. No terminal means no prompt.
- Picking a mode does not count as consent to system integrations. Only an interactive "yes" or `--yes` does.
- `ui::confirm` runs on git's post-command path. It must time out after 60s and must never change git's exit code (see `changelog/interactive-update-prompt.md`).
- Under `curl | sh`, the installer hands `/dev/tty` to onboarding and skips cleanly when no tty can be opened.
- Ctrl-C during a selector exits with code 130 and leaves the terminal usable.

---

## 2. Design principles

1. **One visual language.** Every interactive flow uses the same left-rail style (the layout clack-style CLIs use), the same glyphs and the same colors, all from one theme module.
2. **Answered steps collapse.** Once a step is done it becomes one line. The scrollback ends up as a short, readable record of what was chosen.
3. **Show progress.** Multi-step flows show `step n of N` and give a time estimate that matches reality.
4. **Never leave the user at a dead end.** Every failure screen offers the next action as a choice, such as Retry / Use a token / Continue locally. The user should not have to copy a command out of a paragraph.
5. **Plain mode works fully.** With `NO_COLOR`, `--plain`, no TTY, or a dumb/legacy console, the output is ASCII-only, carries the same information, and never asks a question.
6. **Separate streams.** stderr carries UI and stdout carries data. On a TTY, `whoami` and similar commands may print a formatted card. On a pipe they keep today's line format.

---

## 3. Choosing the TUI package

| Crate | What it is | Fit for autter |
|---|---|---|
| **cliclack 0.5** | Rust port of `@clack/prompts`: intro/outro, select, multiselect, confirm, input, password, spinner, progress and multi-progress, notes and log lines, custom themes | **Best fit for linear flows** such as signup, login and install. It renders on **stderr** (checked in source: `Term::stderr()` in `interaction.rs:61` and `lib.rs:327`). It builds on `console 0.16` and `indicatif 0.18`, which are already in `Cargo.lock`, so it only adds `strsim`, `textwrap` and `zeroize`. |
| inquire 0.9 | Mature prompt library with validators and autocompletion | Good prompts, but no intro/outro or rail layout, so we would still need our own spinner and step chrome. |
| dialoguer | console-based prompts | Simple and dated-looking. It also lacks the rail layout. |
| ratatui 0.30 (already a dependency) | Full widget/layout TUI with `Viewport::Inline` | Too heavy for a series of questions. It is the right tool for real screens, such as an interactive stats or blame browser. |

**Recommendation:**
- Use **cliclack** for every linear interactive flow.
- Put a thin `crate::ui` facade in front of it so call sites never import cliclack directly.
- Keep `ratatui` for Phase 4 (interactive views), which is now committed. It stays out of the linear flows.
- Keep the git post-command `confirm` hand-written. cliclack has no timeout, so the 60s deadline has to stay. Restyle it with the shared theme so it looks the same as the rest.

**Points to verify in the Phase 1 spike:**
- `console` does not check `NO_COLOR` the way we do. Have the facade call `console::set_colors_enabled_stderr(arg_parser::use_color_stderr())` once at startup.
- Find out how cliclack reports Esc and Ctrl-C (an `io::ErrorKind::Interrupted` error), and map that to today's "Cancelled." message and exit code 130.
- Check rendering in Windows PowerShell 5.1 / conhost, Windows Terminal, the VS Code terminal and tmux, and confirm the ASCII theme kicks in where Unicode does not render.

---

## 4. Target screens

The mockups below use the theme's Unicode glyphs. In ASCII mode, `◆ ◇ │ └ ✓` become `* o | \` + `OK`.

### 4.0 Brand header

Commands that set up autter or show your own data open with a three-line header, like Claude Code's startup banner. The left side is the autter logo (the lowercase "a." from `assets/docs/autter.png`) drawn in block characters. The right side has three lines of context.

```
▗▟▀▀▙▟▌   Autter v2.3.0
▐▌  ▐█▌   Connected · you@company.com · acme
▝▜▄▄▛▜▌ ██ ~\Documents\autter-cli · capturing in 3 tools
```

**Colors**
- The "a" uses the terminal's default foreground, so it works on dark and light backgrounds. The navy from the logo would disappear on dark terminals.
- The dot uses the brand teal: `#2EE6C5` when `COLORTERM` is `truecolor` or `24bit`, 256-color index 43 otherwise, and ANSI cyan on 16-color terminals.
- Line 1 shows the product name in bold, then the version dimmed. Lines 2 and 3 are dimmed.

**What the three lines say**

| Command | Line 1 | Line 2 | Line 3 |
|---|---|---|---|
| `autter onboard` | Autter + version | "Setup · about a minute · N steps" | current folder |
| bare `autter` | Autter + version | mode · account · org (or "Local mode · nothing leaves this machine") | current folder · tools capturing |
| `autter login` (success) | account | org | "Cloud sync resumed" or the pending upload count |
| `autter whoami` | account | sign-in state · token renewal as relative time | credential backend |
| `autter doctor` | Autter + version | "Checking your setup" | current folder |

**Where it never appears:** `stats`, `blame`, `log`, `diff`, `show`, the git proxy, the update prompt, `--json` output, piped output, and `checkpoint`. These run often or feed scripts, so the header would just be noise.

**Fallbacks**
- With no TTY, an ASCII-only locale, or a terminal narrower than 50 columns, there's no logo. Line 1 prints as a single plain line, and the other lines print only if they carry information.
- With `NO_COLOR`, `--no-color` or `--plain` (the same global flag), the logo still draws, uncolored.
- The quadrant characters (`▗ ▟ ▙ ▛ ▜ ▝`) are missing from some fonts, Consolas in legacy conhost among them. On a legacy Windows console, use a variant built only from `▀ ▄ █`:
  ```
  ▄█▀▀█▄█
  █    ██
  ▀█▄▄█▀█ ██
  ```
  Treat a console as legacy only when all three signs are missing: no `WT_SESSION`, no `TERM_PROGRAM`, and `COLORTERM` not `truecolor`/`24bit`. Checking `WT_SESSION` alone is wrong: the Claude desktop terminal panel sets `TERM_PROGRAM=claude-desktop` and `COLORTERM=truecolor` but not `WT_SESSION`. Confirm this rule during the Phase 1 terminal-matrix check.

**Implementation:** `ui::header::render(lines: [&str; 3], caps: TermCaps) -> String` is a pure function, snapshot-tested for each fallback, and written to stderr.

### 4.1 Onboarding, connected path

The flow starts with the header from 4.0 (line 2: "Setup · about a minute · 4 steps"), and then the step rail:

```
│
◇  How should Autter run on this machine?                              1/4
│  Connected — sync to your Autter org
│
◆  Sign in                                                             2/4
│  Code   WDJB-MJHT
│  Opened https://app.autter.dev/device in your browser
│
│  enter reopen browser · c copy code · esc continue in local mode
◒  Waiting for approval…  expires in 14:32
```

Pressing `c` copies the code with an OSC 52 escape sequence (`ESC ] 52 ; c ; <base64> BEL`), which needs no new dependency. The terminal doesn't report whether the copy worked, so the hint briefly changes to "copy requested" rather than claiming success, and the code stays on screen either way. Under tmux, wrap the sequence in tmux's passthrough so it reaches the outer terminal.

After approval, the sign-in step collapses to:

```
◇  Signed in as you@company.com · org acme                             2/4
│
◇  Share anonymous usage data?                                         3/4
│  Yes — events are mirrored to ~/.autter/internal/telemetry.log
│
◆  Install integrations for detected tools?                            4/4
│  ◼ Claude Code   ◼ Cursor   ◼ VS Code   ◼ git capture + background service
│  space toggle · enter confirm
```

The integrations step is a multiselect of the tools detected on this machine, with everything checked, so Enter installs everything, as "Yes" does today. Unchecking every item is the same as "Not now". `--yes` still installs everything without asking. To support this, `install_hooks::run` takes an optional allow-list of agent names. `autter install --system` with no list behaves as today. The consent rule doesn't change: only checked items are installed.

Integrations installing (cliclack multi-progress, one row per agent, all on stderr):

```
◇  Installing integrations
│  ✓ Claude Code      hooks installed
│  ✓ Cursor           hooks installed
│  ✓ VS Code          extension installed
│  ✓ git capture      trace2 configured, background service running
│
└  You're all set — authorship is recorded on every commit.

   autter stats           AI vs human share of your latest commit
   autter blame <file>    who wrote each line
   autter dash            open your dashboard
```

### 4.2 Sign-in failure (replaces the error + PAT text + green "local mode" block)

```
▲  Sign-in didn't finish: the code expired.
│
◆  What next?
│  ● Try again with a new code
│  ○ Paste an access token        (Dashboard → Settings → Access Tokens)
│  ○ Continue in local mode       (connect later with `autter onboard --connect`)
```

"Paste an access token" opens a masked `password` input and calls `run_pat_login`. The closing summary reports the fallback as a warning and does not print a green success line.

### 4.3 Telemetry, one short line with the details in the option hints

```
◆  Share anonymous usage data?                                         3/4
│  ● Yes   OS, CPU arch, Autter version, random install ID. Never code, prompts, paths or names.
│  ○ No    Send nothing. Change anytime with `autter telemetry on|off`.
```

"Yes" stays preselected in interactive runs. Non-interactive runs with no flag still default to off.

### 4.4 Everyday commands

- **Bare `autter` on a TTY**: a short status card in place of the help wall. `autter help` still shows every command, and with no TTY bare `autter` still prints the overview. The card must stay fast: read only local state (config, credentials, the last commit's note) and make no network calls. A row that can't be computed quickly is left out.
  ```
  ▗▟▀▀▙▟▌   Autter v2.3.0
  ▐▌  ▐█▌   Connected · you@company.com · acme
  ▝▜▄▄▛▜▌ ██ ~\Documents\autter-cli · capturing in 3 tools

    Last commit a1b2c3d  ████████████░░░░░░░  62% AI · Claude Code
    Pending uploads  none

    autter stats · autter blame <file> · autter dash · autter help
  ```
- **`whoami`**: on a TTY, the header followed by a grouped card (Organizations / Cloud sync), with token expiry shown as relative time ("in 6 days"). On a pipe, today's `Key: value` lines stay exactly as they are.
- **`doctor`**: a live checklist where each section shows a spinner while running and then collapses to ✓ / ▲ / ✗. Fixes appear under the failing row. The summary goes in the outro.
- **`install-hooks`**: the same row-per-agent multi-progress as onboarding, on stderr. When stdout is not a terminal, it also prints a plain one-line-per-agent result to stdout (`claude-code: installed`). Note the stream change in the changelog.
- **Update prompt**: the same `[Y/n]` behavior and timeout, rendered in the shared theme.

---

## 5. Architecture

Turn `src/ui.rs` into a small module tree. Call sites use only `crate::ui`.

```
src/ui/
  mod.rs        re-exports; is_interactive() (stdin+stderr TTY, not CI, not inside an agent)
  theme.rs      glyphs (Unicode/ASCII), colors, cliclack Theme impl, NO_COLOR wiring
  header.rs     the "a." logo header (§4.0): TermCaps detection, full/legacy/plain variants
  flow.rs       Flow { total_steps } → intro(), step(n, title), outro_ok(), outro_warn(report)
  prompt.rs     select / multiselect / confirm / input / password — each takes the
                non-interactive default; Ctrl-C/Esc → Cancelled (exit 130)
  confirm.rs    the existing timed post-command confirm, restyled (no cliclack)
  progress.rs   Spinner + MultiProgress replacing mdm::spinner, stderr only
  render.rs     pure fns returning String (summary, whoami card, doctor rows) for snapshot tests
```

Migration rules:
- `mdm::spinner::Spinner` becomes a re-export of `ui::progress::Spinner` for one release, then gets removed.
- Remove every raw `\x1b[` in `src/commands/` and route it through the theme. A lint test can grep for this.
- Keep everything that decides behavior (`integration_decision`, `BacklogDecision`, the onboarding flags) separate from rendering. The redesign should only change how things look and the order of steps.

### Testing

- **Snapshots (insta):** every `render.rs` function, with color both off and on, plus ASCII mode. `render_finished` already works this way.
- **Non-TTY integration tests:** `autter onboard --local --no-telemetry`, `--connect` with a mocked device flow (mockito), and `whoami` piped. Assert that stdout is unchanged and that nothing blocks.
- **Manual terminal matrix** before release:
  - Windows Terminal, PowerShell 5.1 conhost, VS Code terminal, macOS Terminal, iTerm2, GNOME Terminal, tmux
  - 60-column width
  - `NO_COLOR=1`
  - `curl | sh`, and `install.ps1` piped through `iex`

---

## 6. Phases

| Phase | Scope | Changes behavior? |
|---|---|---|
| **0. Design sign-off** | This doc and the decisions in §7. Optionally record the mockups with VHS/asciinema for review. | No (done) |
| **1. Foundation** | Add cliclack and the `ui/` facade and theme. Add the brand header (§4.0) and its fallbacks. Port `select` and `confirm`. Move Spinner output to stderr. Remove raw escapes from `login.rs`. Wire up the ASCII fallback. Snapshot tests. | Look only |
| **2. Signup flow** | Onboarding with intro, step counter and outro. New sign-in screen (OSC 52 copy, reopen browser, countdown, esc to go local). Failure screen with Retry / Token / Local. Shorter telemetry step (Yes still preselected). Integrations multiselect, with an agent allow-list for `install_hooks::run`. Fixed welcome copy. Installer hand-off so the PATH notice stays the last thing on screen. | Yes: failure handling, per-tool integrations, step copy |
| **3. Usage surfaces** | install-hooks multi-progress (progress on stderr, plain summary on stdout when piped), live doctor checklist, whoami card, restyled update prompt, bare-`autter` status card. | Yes: stderr/stdout split for install-hooks; bare `autter` on a TTY |
| **4. Interactive views (ratatui)** | Full-screen views built on the existing `ratatui` dependency, sharing the theme's colors: `autter stats -i` to browse recent commits by AI share and drill into per-file and per-agent splits, and a blame browser that opens a file with AI/human gutter colors and lets you jump to the prompt behind a line. Only on a TTY, using the alternate screen, with q/Esc to quit. The existing non-interactive commands stay unchanged. Scope this phase in its own short spec before building it. | New features |

**Phase 1 notes (as built):**
- The selector now runs on cliclack. Arrow keys, j/k and Enter work as before. The 1–9, Tab and Space shortcuts are gone because cliclack's key handling isn't extensible. Esc now cancels like Ctrl-C (exit 130).
- cliclack chooses Unicode or ASCII glyphs from `WT_SESSION` alone, so `PromptTheme` overrides every method that draws a glyph and uses autter's own detection instead.
- `cargo run --example ui_preview` shows the header, the selector, the timed confirm and the spinner outcomes in any terminal without touching config. Use it for the terminal matrix.
- The spinner's outcome lines go to stderr on a terminal. Piped stdout keeps the exact lines it had before (Phase 3's stream decision, landed early because the spinner moved).

**Phase 2 notes (as built):**
- Onboarding runs on one rail (`ui::rail`). Questions carry a counter for the path actually taken: the mode question counts the connected path; if the user picks Local, later counters use the shorter total. The non-interactive skip message now prints before the welcome header, not after it.
- The sign-in screen (`ui::signin`) is hand-drawn on crossterm like `confirm`: it redraws the countdown while a background thread polls (`OAuthClient::poll_for_token_until`, which stops polling when Esc is pressed). Enter reopens the browser, `c` sends OSC 52 (`ui::clipboard`), Esc continues locally in onboarding and cancels in `autter login`, and Ctrl-C exits 130. Without a terminal, sign-in prints the URL and code exactly as before.
- After a failed attempt: "Try again with a new code", "Paste an access token" (masked input that validates the token before storing it), and "Continue in local mode" (or "Cancel" in `autter login`). Continuing after a failure is listed as a warning in the summary. Pressing Esc without a failure is treated as a choice, so it isn't listed.
- Integrations are a multiselect of `install_hooks::detect_tools()` plus "Git capture and background service", all checked to start. `install_hooks::run_for_tools` installs only the checked tools, skips the "Coding Agents" header and "not detected" list, and indents spinner output under the rail (`ui::progress::on_rail`). `--yes` still installs everything.
- Onboarding no longer uses unconditional color codes, so `NO_COLOR` and `--no-color` apply to it too.
- `cargo run --example ui_preview` walks the same screens with sample data, including a sign-in that approves itself after six seconds.

Phases 1 and 2 are the signup improvement. Phase 3 is the usage improvement. Phase 4 adds new interactive features. Each phase can ship as its own PR.

---

## 7. Decisions

| # | Question | Decision |
|---|---|---|
| 1 | Telemetry default | **Keep "Yes" preselected** in interactive onboarding. Non-interactive runs with no flag still default to off. |
| 2 | Bare `autter` | **Status card plus key commands** on a TTY, built from local state only. With no TTY it prints the overview, and `autter help` still shows everything. |
| 3 | Per-agent integrations | **Multiselect with everything checked.** `install_hooks::run` gets an agent allow-list, and `--yes` still installs all. |
| 4 | Copying the device code | **OSC 52 on the `c` key.** No new dependency, and the code always stays visible. |
| 5 | ratatui | **Keep it and commit to Phase 4** (interactive stats and blame views). |
| 6 | `install-hooks` output stream | **Progress to stderr, plain summary on stdout when piped.** Nothing in this repo parses that stdout (checked: installers, npm script, editor extensions, tests). Call it out in the changelog. |
