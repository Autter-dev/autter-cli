# `NO_COLOR` / TTY-aware coloring (project-wide)

### Added

- **Centralized `should_colorize()` decision** in `src/commands/arg_parser.rs`,
  building on the global-flag parser:
  - `use_color()` (stdout) and `use_color_stderr()` (stderr) both return `false`
    when the `--no-color`/`--plain` flag is set, the standard `NO_COLOR` env var
    is present, or the relevant stream is not a TTY.
  - `paint(code, text)` / `paint_err(code, text)` helpers wrap text in ANSI SGR
    codes only when coloring is enabled, so call sites stay readable.

### Changed

- Routed all previously-unconditional or ad-hoc colored output through the
  centralized helper so coloring is consistent everywhere:
  - `src/mdm/spinner.rs` — success/pending/error/skipped messages and `print_diff`
  - `src/authorship/stats.rs` — the gray deletion-only bar and the clickable
    "untracked" label (also gated on color)
  - `src/commands/install_hooks.rs` — section headers, dry-run notices, restart
    warnings, and the git-version-too-old box
  - `src/commands/upgrade.rs` — install progress, update notices, and the
    below-minimum-version warning
  - `src/commands/exchange_nonce.rs` — the auto-login confirmation
- `autter log --plain` now also disables autter-rendered color, consistent with
  `--no-color` / `NO_COLOR`.

### Notes

- The parse-stability `--no-color` flags injected into child git invocations
  (`log.rs`, `diff.rs`) are intentionally left untouched — they keep git output
  machine-parseable and are unrelated to user-facing color.
- `status` and `diff` were already routed through the helper in the prior
  global-flag work.
