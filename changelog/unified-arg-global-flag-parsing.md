# Unified argument & global-flag parsing for direct `autter` subcommands

### Added

- **Shared global-flag parser** (`src/commands/arg_parser.rs`) for the direct
  `autter <subcommand>` path. Standardizes the global flags
  `--json`, `--quiet`, `--no-color`, `-C <path>`, and `-h`/`--help` across
  subcommands instead of each handler hand-rolling its own `while i < args.len()`
  loop.
  - `pre_parse(args, ScanMode, recognize_change_dir)` extracts the global flags
    and returns the leftover args for each handler's own parsing. Unknown flags
    fall through untouched, so each subcommand keeps ownership of its grammar and
    error messages. `--` and everything after it passes through verbatim.
  - `ScanMode::Full` (status, stats, file-changes, fetch-notes) vs
    `ScanMode::LeadingOnly` (blame, diff, checkpoint) — `LeadingOnly` stops
    scanning at the first positional so a downstream git-style flag (e.g. blame's
    `-C` copy-detection) or a ref starting with `-` is never stolen.
  - Process-wide resolved-flags singleton with `use_color()` (honors
    `--no-color`, the `NO_COLOR` env var, and TTY), `quiet()`, and `json()`.
- **`-C <path>` support**, matching git's `git -C <path> <cmd>` model: leading
  `-C` runs `autter` as if started in `<path>` (process chdir), so every command
  resolves the target repository and child git inherits the working directory.
- **Per-subcommand `--help`.** Each command now has its own help text via a help
  registry. `autter <cmd> --help`, `autter help <cmd>`, and `autter --help <cmd>`
  all route to the same per-command help; the top-level overview is generated
  from the registry so it can't drift.

### Changed

- Migrated `status`, `file-changes`, `stats`, `fetch-notes`, `checkpoint`,
  `blame`, and `diff` to the shared parser. `blame`/`diff` also honor a global
  `--json` supplied before their positional argument.
- `status` and `diff` color output now goes through the centralized
  `use_color()` decision (respects `--no-color`/`NO_COLOR`/non-TTY); `status`
  suppresses its informational hints under `--quiet`.
- The monolithic top-level help block was replaced by the registry-driven
  overview.

### Notes

- The git proxy dispatch path is untouched — it still passes unknown flags
  straight to git.
- `log` intentionally keeps its own argument handling (it forwards almost
  everything to git and has bespoke `-C`/`--help`/color logic). It still gains
  `autter -C <path> log` from the top-level leading-global handling.
