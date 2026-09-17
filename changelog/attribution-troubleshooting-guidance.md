# Attribution troubleshooting guidance

## Summary

When attribution self-checks fail, `autter doctor` / `autter debug` used to print one generic restart-daemon remediation regardless of failure stage. Missing-authorship footers on `diff` / `blame` / `show` / `stats` only mentioned `install-hooks` and `debug`. Users could see useful low-level evidence but not a clear likely cause or next recovery step.

## Changes

### `src/diagnostics.rs`

- Map attribution self-check errors to stage-specific `fix:` lines with `likely cause:` + concrete next commands (checkpoint persistence, checkpoint command failure, notes/blame mismatch, permissions, daemon `last_error`).
- On failure, keep the leftover self-check repo and print an inspect hint (`cd … && autter blame …`).

### `src/commands/doctor.rs`

- If a sibling trace2 check already failed, prefix the checkpoint round-trip remediation with “run `autter install` first.”

### `src/authorship/guidance.rs`

- Missing-authorship footers now state a likely cause and point at `install-hooks` → `install` → `doctor` → `daemon restart` → `debug`.

### `INSTALL.md`

- New troubleshooting section for attribution check failures.
