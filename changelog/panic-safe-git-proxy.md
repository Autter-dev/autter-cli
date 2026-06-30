# Panic-Safe Git Proxy Instrumentation

## Summary

A panic anywhere in the proxy's instrumentation hooks can abort a user's `git commit`. This change wraps every proxy-side hook in `std::panic::catch_unwind` so that bugs in autter's instrumentation can never prevent the underlying git command from completing.

## Changes

### `src/commands/git_handlers.rs`

- **`run_proxy_hook_guarded(phase, hook)`** — new helper that runs a hook closure under `catch_unwind(AssertUnwindSafe(...))`. Returns `Option<R>`: `Some(value)` on success, `None` on panic. The global panic hook installed in `main` (`observability::install_panic_hook`) runs during unwinding before the catch, so the panic is still reported to Sentry/telemetry — nothing is silently swallowed.
- **`proxy_debug_log(message)`** — new debug logger emitting `[autter] …` to stderr, gated on debug builds or `AUTTER_DEBUG=1`, consistent with the existing logging convention.
- **`maybe_inject_test_panic(phase)`** — debug-only helper (compiled out of release) that panics when `AUTTER_TEST_PANIC_IN_HOOK` matches the given phase, enabling the regression tests below.
- **`handle_git()` refactored** — the two instrumentation regions now run under the guard:
  - **`pre_state`** hook: repo discovery, head-state read, sending pre-state to the daemon. On panic: no `invocation_id` is available, so the proxy falls back to a plain passthrough — real git still runs with no autter state attached, and the command succeeds exactly as if autter were not installed.
  - **`post_state`** hook: post-state send, inline commit stats, min-version warning. On panic: git has already exited, so we skip remaining instrumentation and exit mirroring git's own status — the exit code the user sees is unchanged.

### `tests/daemon_mode.rs`

- **`wrapper_proxy_survives_panic_in_pre_state_hook`** — runs a commit in wrapper mode with `AUTTER_TEST_PANIC_IN_HOOK=pre_state`, asserts the command exits 0 and a new commit SHA appears.
- **`wrapper_proxy_survives_panic_in_post_state_hook`** — same for the `post_state` phase.

## Behaviour Guarantees

| Scenario | Before | After |
|---|---|---|
| Panic in pre-state hook | Process aborts, git never runs | Hook skipped, git runs as plain passthrough, exits with git's code |
| Panic in post-state hook | Process aborts after git, user sees non-zero exit | Hook skipped, exits with git's real exit code |
| No panic | Unchanged | Unchanged |

Sentry/telemetry capture is preserved in both panic scenarios: the global panic hook fires during unwinding before `catch_unwind` catches the payload.

## Notes

- `panic = "abort"` is not set in any Cargo profile, so stack unwinding and `catch_unwind` work correctly in debug, test, and release builds.
- The daemon-side hook pipeline already uses `catch_unwind` throughout (around `route_command`, the checkpoint side-effect pipeline, and listener loops). This change closes the same gap on the proxy side.
- The broader `.unwrap()` → `?` audit across `hooks/`, `checkpoint.rs`, `post_commit.rs`, and `attribution_tracker.rs` is a complementary future improvement. The `catch_unwind` guard now provides a safety net for those call sites on the proxy hot path regardless.
