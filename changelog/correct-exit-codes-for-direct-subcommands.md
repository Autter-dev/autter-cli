## Changed

### Exit codes for user-invoked subcommands

Direct `autter` subcommands now follow the standard CLI exit-code convention:

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | Runtime / IO error (repository not found, git failure, serialization error, etc.) |
| `2` | Usage / argument error (unknown subcommand, missing required flag, bad flag value, etc.) |

This matches the behaviour of `git` itself and makes it straightforward for scripts and CI pipelines to distinguish "I called the command wrong" (`2`) from "the command ran but something failed at runtime" (`1`).

**Affected subcommands** (usage/arg errors now exit `2` instead of `1`):

- `autter <unknown-command>` — unrecognised top-level command
- `autter notes <unknown-subcommand>` — unrecognised `notes` subcommand
- `autter blame` — missing file argument; bad argument parse
- `autter stats` — `--ignore` with no pattern; invalid range format; unknown argument
- `autter git-hooks` — sunset usage (only `remove` is accepted)
- `autter config set/unset/--add` — missing required `<key>` or `<value>`
- `autter ci github|gitlab|local` — missing/unknown subcommand; missing required flags (`--merge-commit-sha`, `--head-sha`, `--base-ref`, etc.)
- `autter bg <unknown-subcommand>` — unrecognised daemon subcommand
- `autter whoami <unknown-args>` — unexpected arguments
- `autter debug <unknown-args>` — unrecognised option
- `autter squash-authorship` — unknown argument; missing positional `base_branch`, `new_sha`, or `old_sha`
- `autter show` — missing revision argument; too many arguments
- `autter file-changes` — `--limit` missing value or non-integer; unknown argument
- `autter telemetry <unknown-subcommand>` — unrecognised subcommand
- `autter fetch-notes` — `--remote` missing value or repeated; unknown option; unexpected argument
- `autter notes migrate` — unknown option
- `autter show-prompt` — argument parse error
- `autter diff` — missing commit/range argument
- `autter upgrade` — unknown argument

### Policy documentation

A new `EXIT_SUCCESS`, `EXIT_RUNTIME_ERROR`, and `EXIT_USAGE_ERROR` constant set in `src/commands/mod.rs` documents and centralises the policy. Every usage-error `exit` in the codebase now references `EXIT_USAGE_ERROR` so the intent is self-evident in code review.

### `checkpoint` is intentionally exempt

`autter checkpoint` runs inside AI-agent editor hooks and the git proxy. A non-zero exit would surface as a failure inside the user's editor or break the agent's edit loop. Attribution is best-effort, so `checkpoint` continues to exit `0` on every failure path. This exception is documented in `src/commands/autter_handlers.rs`.
