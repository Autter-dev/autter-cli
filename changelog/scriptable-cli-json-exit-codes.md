# Scriptable CLI: `--json` everywhere and exit codes CI can act on

## Summary

Pipelines couldn't use autter reliably: most commands had no machine-readable output, and `autter stats` exited 0 when nothing was captured, so CI couldn't tell "nothing captured" from a real result. Every reporting command now speaks one JSON contract, exit codes are documented, and `--strict` turns "nothing captured" and "capture is broken" into failures.

## Added

- **`--json` on every reporting command**: `stats`, `status`, `log`, `show`, `blame`, `diff`, `whoami`, `doctor` and `sync status`. `log`, `show` and `whoami` are new. Stdout carries exactly one JSON document; diagnostics go to stderr. Every document has `schema_version` (1), and fields are only ever added. The JSON Schema is in `docs/schemas/autter-cli.schema.json`.
- **`stats --coverage --json`** carries `schema_version` too.
- **JSON errors**: a failure under `--json` prints `{"schema_version":1,"error":{"code","message","exit_code"}}` and exits non-zero. When the command already built its report, the report's fields stay next to `error`.
- **`stats` fields**: `commit`, `lines` (added, deleted, ai, human, untracked, unknown), `percent` (one decimal) and `capture.status` (`captured`, `nothing_captured`, `no_additions`), next to the existing fields and `record`. Untracked lines are in a commit whose note doesn't attest them; Unknown lines are in a commit with no note. `status` and `log --json` use the same fields.
- **`--strict`** (or `AUTTER_STRICT=1`): `stats` and `status` exit 3 when lines were added and none is attributed to AI or a known human; `doctor` exits 4 when a check capture depends on fails. Without it, behaviour is unchanged.
- **Exit codes in `autter --help`**, the README and `docs/scripting.md`, which also has a GitHub Actions example that fails a pull request when nothing was captured or the AI share is above a limit.

## Changed

- A reporting command run outside a git repository now exits 2 (usage error) instead of 1, still with the one-sentence message, as do `status`, `sync status` and `log` with an unknown flag, and `stats` with a mistyped flag (it used to be treated as a revision).
- `stats --wait --json` on a note that never arrived prints `error` as an object (`note_pending`) instead of a string; `pending`, `commit` and `waited_secs` are unchanged.
- When `CI` is set, autter prints no color, never prompts, shows no spinners or full-screen views and never shows the update notice, even on a pseudo-terminal. `--json` turns color off too.
