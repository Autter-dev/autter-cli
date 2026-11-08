# Usage metrics drain in smaller batches, and say why when they don't

A usage-metrics queue (643 events in the report) could stop draining for good while `autter doctor` said only "durable metrics replay produced no accepted batch". Authorship notes kept reaching the dashboard, so the message made attribution look broken when it wasn't, and restarting the background service changed nothing.

## Why it got stuck

- Every replay sent the same batch: the oldest 1,000 events. If the server couldn't answer within the 30-second client timeout, the next replay sent the same 1,000 events and timed out the same way.
- A timeout wasn't recognised as one. It surfaced as a generic error, and the queue retried it every three seconds with no backoff.
- The real error went only to the daemon log. Metrics failures were stored in the same record as notes and transcript failures, so they showed up as "Cloud sync is failing — your data is not reaching autter" after every commit.

## What changed

- **Failures are classified** (`src/api/metrics.rs`): timeout, HTTP status (413 and 504 count as "too large"; 401/403 as auth; other 4xx as rejected; 5xx as a server error), network error, or an unreadable answer. `src/http.rs` now reports whether a transport failure was a timeout.
- **The batch shrinks after a timeout** (`src/metrics/sync_state.rs`, `src/daemon/telemetry_worker.rs`): a timeout or HTTP 413/504 halves the next batch (1000 → 500 → … → 1). Five successful batches in a row double it again. Other failures keep the size. A single event refused with 413 is still dropped, as before.
- **Exponential backoff**: 5 s after the first failure, doubling up to 15 minutes. While it is backing off, new events go straight to the queue instead of being uploaded (and timing out) on every tick. A failed request ends the replay pass; it does not go on to send the next batch.
- **The last failure is saved** in `~/.autter/internal/metrics-sync-state.json`: endpoint, kind, HTTP status, error, time, batch size and how many attempts in a row have failed, with running totals of uploaded, dropped and failed attempts.
- **`autter doctor`** has a separate **usage metrics upload** check. When it's failing it's a warning, not a failure. It names `POST /worker/metrics/upload` and the error ("timeout after 30s (no response)", "HTTP 413: …"), the attempt count, when it retries next and with how many events, and that attribution is not affected. It never suggests restarting. The **durable sync queue** check now counts only attribution queues (notes, commit summaries, transcripts, file changes).
- **`autter sync status`** shows usage metrics on their own line: queued, uploaded and failed counts, the last error and the fix. A metrics failure no longer makes cloud sync "upload failing" or changes the exit code. `--json` adds a `usage_metrics` object.
- Metrics failures no longer go into the shared cloud-sync failure record, so the post-commit "Cloud sync is failing" warning isn't shown for them. A metrics record left by an older version is ignored.
- `autter sync purge` also resets the metrics state.

## Server (autter-monorepo, `backend/src/routes/cli-data-plane.ts`)

- Batches of more than 1,000 events get HTTP 413. Before, events past the 1,000th were dropped without an error, so the CLI deleted them as uploaded.
- A database error that isn't about a row's data (a dropped connection, a compute restart, a timeout) fails the request instead of splitting the batch down to single rows. That split cost up to 1,999 statements per 1,000 events, minutes at the org database's round-trip time, and reported every event as invalid, which the CLI then deleted.
- Splitting stops after 20 seconds. Rows not written by then come back with `retry later: …`, which the CLI re-queues.
- Out-of-range timestamps are rejected before the insert, and the error reasons (`invalid_timestamp`, …) now read the SQLSTATE through drizzle's error wrapper.
