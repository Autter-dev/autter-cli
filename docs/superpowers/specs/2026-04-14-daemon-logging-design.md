# Daemon Production Logging — Design Spec

**Date:** 2026-04-14
**Status:** Approved

## Problem

Outside of debug mode (`AUTTER_DEBUG=1`), the daemon log file at
`~/.autter/internal/daemon/logs/{PID}.log` receives almost no output. All 581
`debug_log()` calls across the codebase are gated behind `is_debug_enabled()`,
making production debugging nearly impossible. Errors, git operations,
checkpoints, auto-updater activity, and auto-restarts are invisible unless a
developer remembered to set the env var before the issue occurred.

## Solution

Migrate the entire codebase from the hand-rolled `debug_log()` system to the
`tracing` crate with proper log levels, a custom Sentry-forwarding Layer, and
structured production output.

## Tracing Infrastructure

### New Crates

- `tracing = "0.1"` — macros and span/event types
- `tracing-subscriber = { version = "0.3", features = ["env-filter"] }` —
  formatting + level filtering

### Subscriber Setup

Initialized in `run_daemon()` before `maybe_setup_daemon_log_file()`:

```
tracing_subscriber::registry()
    .with(EnvFilter)       // level gating
    .with(fmt::Layer)      // human-readable output to stderr
    .with(SentryLayer)     // routes errors to Sentry
    .init();
```

### Level Filtering

- **Default:** `info` — production events (write ops, checkpoints, updater,
  errors)
- **`AUTTER_DEBUG=1`:** `debug` — equivalent to today's behavior
- **Override:** `RUST_LOG=autter::daemon=debug` for fine-grained control

### Output Format

Compact human-readable text (not JSON). Read by humans via `autter bg tail`.

```
2026-04-14T10:32:01Z INFO  daemon started pid=12345 version=1.3.0
2026-04-14T10:32:01Z INFO  update check: no update available
2026-04-14T10:45:12Z INFO  commit pre  repo=/home/user/project ref=HEAD
2026-04-14T10:45:12Z INFO  commit post repo=/home/user/project ref=abc1234
2026-04-14T10:45:13Z INFO  checkpoint  kind=Human repo=/home/user/project
2026-04-14T11:32:01Z ERROR trace ingest panic: index out of bounds
```

### Log File Routing

The existing `maybe_setup_daemon_log_file()` calls
`dup2(log_fd, STDERR_FILENO)` to redirect fd 2 to the log file. The tracing
`fmt::Layer` writes to stderr (fd 2). After dup2, writes go to the file. No
`tracing-appender` needed. The existing redirect mechanism works as-is.

## Sentry Routing

### Custom Tracing Layer

A thin `tracing::Layer` impl (~50 lines) that intercepts ERROR-level events
and forwards them to the existing telemetry worker via
`TelemetryEnvelope::Error` → `submit_daemon_internal_telemetry()`.

Every `tracing::error!(...)` in daemon code automatically goes to both the log
file (via fmt Layer) AND Sentry (via this Layer -> telemetry worker -> both
enterprise and OSS DSNs). No manual `send_telemetry_event()` needed at error
call sites.

### What Stays in the Telemetry Worker

- `TelemetryEnvelope::Error` — still needed for wrapper-process errors arriving
  via control socket
- `TelemetryEnvelope::Performance` — explicit perf measurements, not log events
- `TelemetryEnvelope::Message` — PostHog routing
- `TelemetryEnvelope::Metrics` and CAS — unchanged

### What Gets Removed from Daemon Code

All manual `submit_daemon_internal_telemetry(vec![TelemetryEnvelope::Error
{...}])` calls that exist alongside `debug_log()` at error sites. The Layer
handles Sentry routing automatically now.

### Future Path to Standard Sentry

The tracing infrastructure is the foundation. When ready, the custom Layer can
be swapped for `sentry-tracing` and the custom `SentryClient` for the official
`sentry` crate — same subscriber, same tracing calls, just a different Layer.

## Production Log Event Inventory

### ERROR (log file + Sentry via Layer)

- Panic catches: trace ingest, command side-effect, checkpoint side-effect
- Socket listener failures (control socket, trace socket exit with error)
- Log file setup failure
- Telemetry flush panic

### WARN (log file only)

- Update check HTTP/parse failure
- Stale checkpoint pruning failure
- CAS upload failure
- Metrics upload failure (before SQLite fallback)
- Telemetry envelope parse errors

### INFO (log file only)

**Lifecycle:**
- Daemon started (PID, version, platform)
- Daemon shutdown initiated (reason: signal / update / max-uptime)
- Daemon shutdown complete

**Auto-restart:**
- `"uptime exceeded 24.5h, requesting restart"` when max-uptime triggers

**Auto-updater (verbose — all steps):**
- Update check started
- Update check result: no update / newer version found (with version)
- Update download started
- Update install completed / failed
- Post-shutdown self-update: started, completed, failed

**Git write ops (pre/post pairs):**
- commit, rebase, merge, cherry-pick, amend, stash, reset, push
- One line each: `{op} pre repo=<path>` / `{op} post repo=<path>`

**Git read ops — NOT logged at INFO:**
- status, diff, log, show, fetch, checkout (no-write) — stay at DEBUG

**Checkpoints (concise):**
- `checkpoint start kind=<Kind> repo=<path>`
- `checkpoint done  kind=<Kind> repo=<path> duration_ms=<N>`
- Two lines per checkpoint. Verbose state dumps stay at DEBUG.

**Control socket:**
- Connection accepted from new family (first time a repo connects)
- Not per-message

### DEBUG (log file only when AUTTER_DEBUG=1)

- All existing `debug_log()` content (581 calls converted as-is)
- Detailed checkpoint state / watermark updates
- Trace event normalization details
- Family actor state transitions
- Git read ops
- CAS upload success details
- Performance logging (replaces `debug_performance_log`)

## debug_log() Migration

### Scope

Codebase-wide. All 581 `debug_log()` calls across 44 files, plus 15
`debug_performance_log()` and 2 `debug_performance_log_structured()` calls.
One call pattern across the entire codebase.

### Conversion Rules

| Current | New |
|---------|-----|
| `debug_log(...)` at error sites (panics, socket failures) | `tracing::error!(...)` |
| `debug_log(...)` at warning sites (update failures, pruning failures) | `tracing::warn!(...)` |
| `debug_log(...)` at key operation sites (updater, restart, lifecycle) | `tracing::info!(...)` |
| `debug_log(...)` everywhere else | `tracing::debug!(...)` |
| `debug_performance_log(...)` | `tracing::debug!(...)` |
| `debug_performance_log_structured(...)` | `tracing::debug!(...)` |

### What Gets Removed

- `debug_log()` function in `utils.rs`
- `debug_performance_log()` function in `utils.rs`
- `debug_performance_log_structured()` function in `utils.rs`
- `is_debug_enabled()`, `DEBUG_ENABLED` OnceLock
- `is_debug_performance_enabled()`, `debug_performance_level()`,
  `DEBUG_PERFORMANCE_LEVEL` OnceLock

### What Does NOT Change

- `eprintln!()` calls in CLI handlers — these are user-facing terminal output,
  not logging. They stay as-is.
- The `dup2` redirect mechanism in `maybe_setup_daemon_log_file()`
- Log file pruning (removes logs from dead PIDs older than 1 week)
- Log path: `~/.autter/internal/daemon/logs/{PID}.log`
- `autter bg tail` — continues to work as-is

### Non-Daemon Code

tracing macros are no-ops when no subscriber is installed. Non-daemon code
(CLI commands, wrapper processes) that calls `tracing::debug!(...)` will
simply produce no output — same behavior as today's `debug_log()` when
`AUTTER_DEBUG` is unset. If a future PR wants those processes to log, it
just needs to initialize a subscriber there too.

## Log File Management

No changes to existing mechanism:
- `dup2` redirect stays
- Log pruning stays (1 week TTL for dead PIDs)
- Log path unchanged
- `autter bg tail` unchanged

## Future Improvements (Out of Scope)

- Migrate to official `sentry` crate with `sentry-tracing` Layer
- Convert `TelemetryEnvelope::Performance` to tracing spans
- Add tracing subscriber initialization for non-daemon processes
- Structured JSON log output option
- Log rotation by size
