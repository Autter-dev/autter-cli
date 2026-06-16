# Daemon Production Logging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate the entire codebase from `debug_log()` to `tracing` with proper log levels, adding production-visible logging for daemon operations and a custom Sentry-forwarding Layer.

**Architecture:** Add `tracing` + `tracing-subscriber` crates. Initialize a subscriber in the daemon with an `EnvFilter` (default `info`, `debug` when `AUTTER_DEBUG=1`), a `fmt::Layer` writing to stderr (which is dup2'd to the daemon log file), and a custom `SentryLayer` that intercepts ERROR events and routes them to the existing telemetry worker. Convert all 581 `debug_log()` calls codebase-wide, promoting key events to info/warn/error levels.

**Tech Stack:** Rust, `tracing 0.1`, `tracing-subscriber 0.3` (with `env-filter` feature)

---

### Task 1: Add tracing dependencies to Cargo.toml

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add tracing and tracing-subscriber dependencies**

Add these two lines to the `[dependencies]` section of `Cargo.toml`, after the existing `tokio` line:

```toml
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [ ] **Step 2: Verify the project compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: compiles successfully (warnings are OK)

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "feat(logging): add tracing and tracing-subscriber dependencies"
```

---

### Task 2: Create the custom SentryLayer

**Files:**
- Create: `src/daemon/sentry_layer.rs`
- Modify: `src/daemon/mod.rs` (or the inline module declarations in `src/daemon.rs` if that's how submodules are declared)

This Layer intercepts ERROR-level tracing events and forwards them to the existing `TelemetryEnvelope::Error` pipeline via `submit_daemon_internal_telemetry()`.

- [ ] **Step 1: Check how daemon submodules are declared**

Read `src/daemon.rs` lines 1-30 to find the module declarations (e.g., `mod control_api;`, `mod telemetry_worker;`). The new module `sentry_layer` will be added alongside them.

- [ ] **Step 2: Create `src/daemon/sentry_layer.rs`**

```rust
//! Custom tracing Layer that forwards ERROR-level events to Sentry
//! via the existing daemon telemetry worker pipeline.

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// A tracing Layer that intercepts ERROR-level events and routes them
/// to the daemon's telemetry worker as `TelemetryEnvelope::Error` events,
/// which get forwarded to both enterprise and OSS Sentry DSNs.
pub struct SentryLayer;

struct MessageVisitor {
    message: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl MessageVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            fields: serde_json::Map::new(),
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(format!("{:?}", value)),
            );
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }
}

impl<S: Subscriber> Layer<S> for SentryLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() != Level::ERROR {
            return;
        }

        let mut visitor = MessageVisitor::new();
        event.record(&mut visitor);

        let context = if visitor.fields.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(visitor.fields))
        };

        let envelope = crate::daemon::control_api::TelemetryEnvelope::Error {
            timestamp: chrono::Utc::now().to_rfc3339(),
            message: visitor.message,
            context,
        };

        crate::daemon::telemetry_worker::submit_daemon_internal_telemetry(vec![envelope]);
    }
}
```

- [ ] **Step 3: Add the module declaration**

In the file where daemon submodules are declared, add:

```rust
pub mod sentry_layer;
```

- [ ] **Step 4: Verify the project compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: compiles successfully

- [ ] **Step 5: Commit**

```bash
git add src/daemon/sentry_layer.rs src/daemon.rs
git commit -m "feat(logging): add custom SentryLayer for tracing error forwarding"
```

---

### Task 3: Initialize tracing subscriber in daemon startup

**Files:**
- Modify: `src/daemon.rs` (the `run_daemon()` function, around line 7681)

The subscriber must be initialized BEFORE `maybe_setup_daemon_log_file()` sets up the dup2 redirect, so the fmt Layer captures the stderr handle (fd 2). After dup2, writes through that handle go to the log file.

- [ ] **Step 1: Add tracing initialization to `run_daemon()`**

In `src/daemon.rs`, in the `run_daemon()` function, add the subscriber setup right after `write_pid_metadata(&config)?;` and before `let _log_guard = maybe_setup_daemon_log_file(&config);` (around line 7695-7696).

Add these imports at the top of the function or in the file's import section:

```rust
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
```

Then insert the initialization:

```rust
    // Initialize tracing subscriber before log file redirect so the fmt layer
    // captures stderr (fd 2). After dup2, writes go to the daemon log file.
    let env_filter = if std::env::var("AUTTER_DEBUG").as_deref() == Ok("1") {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_thread_ids(false)
                .with_ansi(false),
        )
        .with(crate::daemon::sentry_layer::SentryLayer)
        .init();
```

Note: `.with_ansi(false)` because the output goes to a log file, not a terminal.

- [ ] **Step 2: Add daemon lifecycle log lines**

Immediately after the subscriber init and the dup2 redirect (`let _log_guard = maybe_setup_daemon_log_file(&config);`), add:

```rust
    tracing::info!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "daemon started"
    );
```

At the end of `run_daemon()`, just before `Ok(())`, add:

```rust
    tracing::info!("daemon shutdown complete");
```

- [ ] **Step 3: Verify the project compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: compiles successfully

- [ ] **Step 4: Commit**

```bash
git add src/daemon.rs
git commit -m "feat(logging): initialize tracing subscriber in daemon startup"
```

---

### Task 4: Migrate daemon.rs debug_log() calls with level promotions

**Files:**
- Modify: `src/daemon.rs`

This is the largest task — converting all ~75 `debug_log()` calls in `daemon.rs`. Most become `tracing::debug!()`, but key events get promoted per the spec.

- [ ] **Step 1: Replace all debug_log calls in daemon.rs**

Apply these conversions throughout `daemon.rs`. The general pattern:

**Replace `debug_log(&format!("...", args))` with `tracing::debug!("...", args)`** — note that tracing macros use `{}` format args directly, so `&format!(...)` is unnecessary.

**Promotions to ERROR (panics, socket failures, log file setup):**

These lines become `tracing::error!(...)`:

- Line ~4611: `debug_log(&format!("daemon trace ingest error: {}", error))` → `tracing::error!(%error, "trace ingest error")`
- Line ~4637: `debug_log(&format!("daemon trace ingest panic: {}", panic_msg))` → `tracing::error!(panic_msg = %panic_msg, "trace ingest panic")`
- Line ~5444: `debug_log(&format!("daemon command side effect failed..."))` → `tracing::error!(family, seq = applied.seq, %error, "command side effect failed")`
- Line ~5464: `debug_log(&format!("daemon command apply failed..."))` → `tracing::error!(family, order, %error, "command apply failed")`
- Line ~5483: `debug_log(&format!("daemon command side effect panic..."))` → `tracing::error!(family, order, panic_msg = %panic_msg, "command side effect panic")`
- Line ~5563: `debug_log(&format!("daemon checkpoint side effect panic..."))` → `tracing::error!(family, order, panic_msg = %panic_msg, "checkpoint side effect panic")`
- Line ~7721: `debug_log(&format!("daemon control listener exited with error: {}", e))` → `tracing::error!(%e, "control listener exited with error")`
- Line ~7724: `debug_log("daemon control listener panicked")` → `tracing::error!("control listener panicked")`
- Line ~7740: `debug_log(&format!("daemon trace listener exited with error: {}", e))` → `tracing::error!(%e, "trace listener exited with error")`
- Line ~7743: `debug_log("daemon trace listener panicked")` → `tracing::error!("trace listener panicked")`

Any line with `"daemon log file setup failed"` → `tracing::error!(...)`
Any line in the telemetry flush with panic → `tracing::error!(...)`

**Promotions to WARN (update failures, pruning failures):**

- Line ~7645: `debug_log(&format!("daemon update check failed: {}", err))` → `tracing::warn!(%err, "update check failed")`
- Line ~7688: `debug_log(&format!("daemon stale captured checkpoint pruning failed: {}", error))` → `tracing::warn!(%error, "stale captured checkpoint pruning failed")`
- Line ~5456: `debug_log(&format!("daemon command completion log write failed..."))` → `tracing::warn!(family, order, %error, "command completion log write failed")`

**Promotions to INFO (updater, restart, lifecycle):**

- Line ~7637: `debug_log("daemon update check: newer version available, requesting shutdown")` → `tracing::info!("update check: newer version available, requesting shutdown")`
- Line ~7642: `debug_log("daemon update check: no update needed")` → `tracing::info!("update check: no update needed")`
- Line ~7651: `debug_log("daemon uptime exceeded max, requesting restart")` → `tracing::info!("uptime exceeded max, requesting restart")`
- Line ~7670: `debug_log("daemon self-update: installation completed successfully")` → `tracing::info!("self-update: installation completed successfully")`
- Line ~7673: `debug_log("daemon self-update: no update to install")` → `tracing::info!("self-update: no update to install")`
- Line ~7676: `debug_log(&format!("daemon self-update: installation failed: {}", err))` → `tracing::warn!(%err, "self-update: installation failed")`

**Everything else stays DEBUG:**

All remaining `debug_log(...)` calls → `tracing::debug!(...)`.

- [ ] **Step 2: Remove the `observability::log_error()` calls that duplicate error tracing**

In daemon.rs, wherever there's a pattern of `debug_log(error) + observability::log_error(error, context)` at error/panic sites, the `observability::log_error()` call is now redundant because the `SentryLayer` automatically forwards `tracing::error!()` to Sentry. **However**, the `observability::log_error()` calls include structured `context` JSON with fields like `component`, `phase`, `reason`, `panic_message`. To preserve this context, include those fields as tracing event fields:

```rust
// Before:
debug_log(&format!("daemon trace ingest panic: {}", panic_msg));
observability::log_error(&error, Some(serde_json::json!({
    "component": "daemon",
    "phase": "trace_ingest_worker",
    "reason": "panic_in_ingest",
    "panic_message": panic_msg,
})));

// After:
tracing::error!(
    component = "daemon",
    phase = "trace_ingest_worker",
    reason = "panic_in_ingest",
    panic_msg = %panic_msg,
    "trace ingest panic"
);
// The SentryLayer picks up all fields and sends them as Sentry context.
// Remove the observability::log_error() call.
```

Apply this pattern to all ~16 `observability::log_error()` call sites in `daemon.rs`.

- [ ] **Step 3: Remove `use crate::utils::debug_log;` import from daemon.rs**

After converting all calls, remove the import. The compiler will tell you if any were missed.

- [ ] **Step 4: Verify the project compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: compiles successfully (possibly with warnings about unused imports in other files)

- [ ] **Step 5: Commit**

```bash
git add src/daemon.rs
git commit -m "feat(logging): migrate daemon.rs from debug_log to tracing with level promotions"
```

---

### Task 5: Add new INFO-level log points for git write ops and checkpoints

**Files:**
- Modify: `src/daemon.rs`

Add the production-visible log points that don't currently exist: git write op pre/post pairs and concise checkpoint logging.

- [ ] **Step 1: Add git write op INFO logging**

In `maybe_apply_side_effects_for_applied_command()` (line ~6500), near the top after `let parsed_invocation = ...` (line ~6517), add INFO-level pre/post logging for write operations:

```rust
    // Log write operations at INFO level for production visibility.
    let primary = cmd.primary_command.as_deref().unwrap_or("unknown");
    let is_write_op = matches!(
        primary,
        "commit" | "rebase" | "merge" | "cherry-pick" | "am" | "stash" | "reset" | "push"
    );
    if is_write_op && cmd.exit_code == 0 {
        let repo_path = cmd.worktree.as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let post_head = cmd.post_repo.as_ref()
            .and_then(|r| r.head.clone())
            .unwrap_or_default();
        tracing::info!(
            op = primary,
            repo = %repo_path,
            new_head = %post_head,
            "git write op completed"
        );
    }
```

Note: We log at the end of side-effect application (post) with the result, rather than pre/post pairs, to keep it to one line. The pre-state is implicit (it's the command being processed). If separate pre/post is desired, add the pre log before `self.coordinator.route_command()` in the `FamilySequencerEntry::ReadyCommand` branch.

- [ ] **Step 2: Add checkpoint INFO logging**

In the `FamilySequencerEntry::Checkpoint` branch (line ~5501), add concise pre/post logging around `apply_checkpoint_side_effect()`:

Before the checkpoint execution (around line 5537, before the `catch_unwind`):

```rust
    let checkpoint_kind_str = match request.as_ref() {
        CheckpointRunRequest::Live(req) => req.kind.as_deref().unwrap_or("human"),
        CheckpointRunRequest::Captured(_) => "captured",
    };
    tracing::info!(
        kind = checkpoint_kind_str,
        repo = %repo_wd,
        "checkpoint start"
    );
    let checkpoint_start = std::time::Instant::now();
```

After the checkpoint completes (after the `match checkpoint_result` block resolves `result`, around line 5586):

```rust
    let checkpoint_duration_ms = checkpoint_start.elapsed().as_millis();
    if result.is_ok() {
        tracing::info!(
            kind = checkpoint_kind_str,
            repo = %repo_wd,
            duration_ms = checkpoint_duration_ms,
            "checkpoint done"
        );
    } else {
        tracing::warn!(
            kind = checkpoint_kind_str,
            repo = %repo_wd,
            duration_ms = checkpoint_duration_ms,
            "checkpoint failed"
        );
    }
```

- [ ] **Step 3: Add shutdown reason logging**

In `request_shutdown()` or wherever the coordinator's shutdown is triggered, and in the update check loop where shutdown is requested, add:

```rust
// In daemon_update_check_loop, update-available path:
tracing::info!("shutdown requested: update available");

// In daemon_update_check_loop, max-uptime path:
tracing::info!("shutdown requested: uptime exceeded max");
```

These replace the `debug_log` calls that were already promoted to info — just ensure the message clearly states the reason.

- [ ] **Step 4: Verify the project compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: compiles successfully

- [ ] **Step 5: Commit**

```bash
git add src/daemon.rs
git commit -m "feat(logging): add INFO-level logging for git write ops, checkpoints, and shutdown reasons"
```

---

### Task 6: Migrate telemetry_worker.rs debug_log() calls

**Files:**
- Modify: `src/daemon/telemetry_worker.rs`

- [ ] **Step 1: Convert all debug_log calls in telemetry_worker.rs**

There are 5 `debug_log()` calls in this file. Convert them:

- `debug_log(&format!("telemetry flush task panicked: {}", e))` (line ~255) → `tracing::error!(%e, "telemetry flush task panicked")`
- `debug_log("daemon telemetry: skipping CAS flush, not logged in")` (line ~522) → `tracing::debug!("telemetry: skipping CAS flush, not logged in")`
- `debug_log(&format!("daemon telemetry: CAS parse error: {}", e))` (line ~534) → `tracing::warn!(%e, "telemetry: CAS parse error")`
- `debug_log(&format!("daemon telemetry: uploaded {} CAS objects", chunk.len()))` (line ~568) → `tracing::debug!(count = chunk.len(), "telemetry: uploaded CAS objects")`
- `debug_log(&format!("daemon telemetry: CAS upload error: {}", e))` (line ~573) → `tracing::warn!(%e, "telemetry: CAS upload error")`

- [ ] **Step 2: Remove the `use crate::utils::debug_log;` import**

- [ ] **Step 3: Verify the project compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: compiles successfully

- [ ] **Step 4: Commit**

```bash
git add src/daemon/telemetry_worker.rs
git commit -m "feat(logging): migrate telemetry_worker.rs from debug_log to tracing"
```

---

### Task 7: Migrate all remaining debug_log() calls codebase-wide

**Files:**
- Modify: All 42 remaining files that use `debug_log()`, `debug_performance_log()`, or `debug_performance_log_structured()`

This is a mechanical conversion — every remaining `debug_log(...)` becomes `tracing::debug!(...)`, every `debug_performance_log(...)` becomes `tracing::debug!(...)`, every `debug_performance_log_structured(json)` becomes `tracing::debug!(%json, "performance")`.

- [ ] **Step 1: Convert authorship module files**

Files (highest call counts first):
- `src/authorship/rebase_authorship.rs` — 43 `debug_log` + 9 `debug_performance_log` → all `tracing::debug!`
- `src/authorship/prompt_utils.rs` — 12 → `tracing::debug!`
- `src/authorship/virtual_attribution.rs` — 9 → `tracing::debug!`
- `src/authorship/range_authorship.rs` — 8 → `tracing::debug!`
- `src/authorship/post_commit.rs` — 6 → `tracing::debug!`
- `src/authorship/attribution_tracker.rs` — 3 → `tracing::debug!`
- `src/authorship/internal_db.rs` — 3 → `tracing::debug!`
- `src/authorship/pre_commit.rs` — 2 → `tracing::debug!`
- `src/authorship/autter_hooks.rs` — 9 → `tracing::debug!`
- `src/authorship/stats.rs` — 1 → `tracing::debug!`

For each file:
1. Replace `debug_log(&format!("...", args))` with `tracing::debug!("...", args)`
2. Replace `debug_log("literal")` with `tracing::debug!("literal")`
3. Replace `debug_performance_log(&format!("...", args))` with `tracing::debug!("...", args)`
4. Remove `use crate::utils::debug_log;` and/or `use crate::utils::debug_performance_log;`

- [ ] **Step 2: Convert commands/hooks module files**

Files:
- `src/commands/hooks/cherry_pick_hooks.rs` — 46 → `tracing::debug!`
- `src/commands/hooks/rebase_hooks.rs` — 39 → `tracing::debug!`
- `src/commands/checkpoint.rs` — 37 → `tracing::debug!`
- `src/commands/checkpoint_agent/bash_tool.rs` — 38 → `tracing::debug!`
- `src/commands/hooks/stash_hooks.rs` — 23 → `tracing::debug!`
- `src/commands/hooks/reset_hooks.rs` — 23 → `tracing::debug!`
- `src/commands/hooks/fetch_hooks.rs` — 22 → `tracing::debug!`
- `src/commands/hooks/checkout_hooks.rs` — 13 → `tracing::debug!`
- `src/commands/hooks/switch_hooks.rs` — 11 → `tracing::debug!`
- `src/commands/ci_handlers.rs` — 10 → `tracing::debug!`
- `src/commands/hooks/push_hooks.rs` — 7 → `tracing::debug!`
- `src/commands/checkpoint_agent/agent_presets.rs` — 7 → `tracing::debug!`
- `src/commands/show_prompt.rs` — 6 → `tracing::debug!`
- `src/commands/git_handlers.rs` — 6 → `tracing::debug!`
- `src/commands/prompts_db.rs` — 10 → `tracing::debug!`
- `src/commands/daemon.rs` — 2 → `tracing::debug!`
- `src/commands/autter_handlers.rs` — 2 → `tracing::debug!`
- `src/commands/hooks/commit_hooks.rs` — 1 → `tracing::debug!`
- `src/commands/hooks/merge_hooks.rs` — 1 → `tracing::debug!`
- `src/commands/hooks/clone_hooks.rs` — 6 → `tracing::debug!`
- `src/commands/hooks/plumbing_rewrite_hooks.rs` — 1 → `tracing::debug!`
- `src/commands/hooks/update_ref_hooks.rs` — 2 → `tracing::debug!`
- `src/commands/git_hook_handlers.rs` — 1 → `tracing::debug!`
- `src/commands/checkpoint_agent/amp_preset.rs` — 1 → `tracing::debug!`
- `src/commands/checkpoint_agent/opencode_preset.rs` — 1 → `tracing::debug!`

- [ ] **Step 3: Convert git module files**

Files:
- `src/git/sync_authorship.rs` — 25 → `tracing::debug!`
- `src/git/repo_storage.rs` — 6 → `tracing::debug!`
- `src/git/refs.rs` — 4 → `tracing::debug!`
- `src/git/repository.rs` — 1 → `tracing::debug!`

- [ ] **Step 4: Convert remaining files**

Files:
- `src/mdm/agents/jetbrains.rs` — 5 → `tracing::debug!`
- `src/mdm/jetbrains/download.rs` — 6 → `tracing::debug!`
- `src/mdm/agents/cursor.rs` — 1 → `tracing::debug!`
- `src/mdm/agents/vscode.rs` — 1 → `tracing::debug!`
- `src/mdm/jetbrains/detection.rs` — 1 → `tracing::debug!`
- `src/mdm/utils.rs` — 1 → `tracing::debug!`
- `src/config.rs` — 1 → `tracing::debug!`
- `src/daemon/trace_normalizer.rs` — 0 `debug_log` but check for any
- `src/observability/wrapper_performance_targets.rs` — 4 `debug_performance_log` + 2 `debug_performance_log_structured` → `tracing::debug!`

- [ ] **Step 5: Verify the project compiles with no debug_log references**

Run: `cargo check 2>&1 | tail -10`
Expected: compiles successfully

Then verify no debug_log calls remain:
Run: `grep -r "debug_log\|debug_performance_log" src/ --include="*.rs" | grep -v "^src/utils.rs" | grep -v "// " | grep -v "test" | head -20`
Expected: no matches (except possibly utils.rs itself and test code)

- [ ] **Step 6: Commit**

```bash
git add src/
git commit -m "feat(logging): migrate all remaining debug_log calls to tracing codebase-wide"
```

---

### Task 8: Remove debug_log infrastructure from utils.rs

**Files:**
- Modify: `src/utils.rs`

- [ ] **Step 1: Remove the debug logging functions and supporting code**

Remove from `src/utils.rs`:
- `static DEBUG_ENABLED: std::sync::OnceLock<bool>` (line 10)
- `static DEBUG_PERFORMANCE_LEVEL: std::sync::OnceLock<u8>` (line 11)
- `fn is_debug_enabled()` (lines 15-22)
- `fn is_debug_performance_enabled()` (lines 24-26)
- `fn debug_performance_level()` (lines 28-35)
- `pub fn debug_performance_log(msg: &str)` (lines 37-41)
- `pub fn debug_performance_log_structured(json: serde_json::Value)` (lines 43-47)
- `pub fn debug_log(msg: &str)` (lines 57-61) and its doc comment (lines 49-56)

Keep `IS_TERMINAL`, `IS_IN_BACKGROUND_AGENT`, and all other functions in utils.rs.

- [ ] **Step 2: Verify the project compiles cleanly**

Run: `cargo check 2>&1 | tail -10`
Expected: compiles successfully with no errors related to missing `debug_log`

- [ ] **Step 3: Verify no remaining references**

Run: `grep -rn "utils::debug_log\|utils::debug_performance" src/ --include="*.rs" | head -10`
Expected: no matches

- [ ] **Step 4: Commit**

```bash
git add src/utils.rs
git commit -m "feat(logging): remove debug_log infrastructure from utils.rs"
```

---

### Task 9: Update SentryLayer to include context fields from observability::log_error patterns

**Files:**
- Modify: `src/daemon/sentry_layer.rs`

The SentryLayer needs to capture all tracing event fields (not just `message`) so that structured context like `component`, `phase`, `reason` flows through to Sentry.

- [ ] **Step 1: Verify the SentryLayer captures all fields**

Review `src/daemon/sentry_layer.rs` — the `MessageVisitor` already collects non-message fields into a `serde_json::Map` and passes them as `context` in the `TelemetryEnvelope::Error`. Verify this matches the pattern used in Task 4 Step 2.

If additional field types need handling (e.g., `record_i64`, `record_u64`, `record_bool`), add them:

```rust
impl Visit for MessageVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Bool(value),
        );
    }

    // ... existing record_debug and record_str methods
}
```

- [ ] **Step 2: Verify the project compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: compiles successfully

- [ ] **Step 3: Commit (if changes were needed)**

```bash
git add src/daemon/sentry_layer.rs
git commit -m "feat(logging): ensure SentryLayer captures all field types for Sentry context"
```

---

### Task 10: Final verification and cleanup

**Files:**
- All modified files

- [ ] **Step 1: Run full compilation check**

Run: `cargo check 2>&1 | tail -20`
Expected: compiles cleanly

- [ ] **Step 2: Run the test suite**

Run: `cargo test 2>&1 | tail -30`
Expected: all tests pass

- [ ] **Step 3: Verify no debug_log or debug_performance_log references remain**

Run: `grep -rn "debug_log\b\|debug_performance_log\b" src/ --include="*.rs" | grep -v "^Binary" | head -20`
Expected: no matches

- [ ] **Step 4: Verify the daemon log format**

Run a quick manual verification that the log output format matches the spec:

```bash
cargo build 2>&1 | tail -3
```

Verify the binary builds. A full E2E test of the daemon logging would require running the daemon, which is tested by existing integration tests.

- [ ] **Step 5: Run existing integration tests**

Run: `cargo test --test daemon_mode 2>&1 | tail -20`
Expected: tests pass (daemon integration tests exercise the daemon startup/shutdown path)

- [ ] **Step 6: Final commit for any cleanup**

If any cleanup was needed:
```bash
git add -A
git commit -m "chore(logging): final cleanup after tracing migration"
```
