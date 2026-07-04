# CLI-Only Features

Features that live entirely inside the `autter` binary. **No backend, Postgres, or web-app
changes required** — each can ship in an independent release. These harden reliability, make the
CLI fully scriptable, and add self-diagnostics.

> Conventions used below: file paths are relative to the repo root. "Proxy path" = the
> `argv[0] == "git"` dispatch (`commands::git_handlers::handle_git`); "direct path" = the
> `argv[0] == "autter"` dispatch (`commands::autter_handlers::handle_autter`).

---

## C1 — Correct exit codes for direct subcommands

**What & why.** Today several error paths in direct subcommands call `std::process::exit(0)`
(e.g. `handle_checkpoint` in `src/commands/autter_handlers.rs`: bad `--hook-input`, unknown
preset, empty stdin all exit `0`). Exiting `0` on error is correct for the **proxy/hook** path
(never break the user's `git`) but wrong for **user-invoked subcommands** run by scripts/CI —
it hides failures. There are 29 `exit(0)` vs 120 `exit(1)` across `src/commands/`; the `exit(0)`
calls on error/usage paths are the bug.

**Builds on.** `src/commands/autter_handlers.rs` dispatch; per-command `handle_*` functions.

**CLI changes.**
- Define a policy: **hook/proxy/checkpoint code → `exit(0)` on internal failure** (degrade
  silently). **Direct user commands → non-zero on usage error (`2`), IO/runtime error (`1`).**
- Audit every `exit(0)` in `src/commands/*.rs`; reclassify the error/usage ones to `1`/`2`.
  Keep `checkpoint`'s hook-input failures at `0` (it runs inside agent hooks) but document why.
- Standardize: `2` = usage/arg error, `1` = runtime error, `0` = success, matching common CLIs.

**Data flow.** Internal — affects only process exit status.

**Effort.** S. **Depends on.** none. **Risk.** low (behavioral change only on already-failing paths).

---

## C2 — Panic-safe git proxy

**What & why.** There are ~2,500 `.unwrap()`/`.expect()` calls in `src/`. A panic on the proxy
or checkpoint hot path can abort a user's `git commit`. The proxy must **always** fall through to
real `git` rather than panic.

**Builds on.** Proxy entrypoint (`commands::git_handlers::handle_git`), checkpoint hot path
(`src/daemon/checkpoint.rs`, `src/authorship/post_commit.rs`, `src/authorship/attribution_tracker.rs`).

**CLI changes.**
- Wrap the proxy's pre/post-hook execution in `std::panic::catch_unwind`. On panic: log to the
  debug log (`debug_log()`), optionally capture to Sentry (already wired), and **continue to
  exec real git** so the user's command succeeds regardless.
- Audit and remove `.unwrap()` in the hottest files (`hooks/`, `daemon/checkpoint.rs`,
  `post_commit.rs`, `attribution_tracker.rs`); replace with `?`/`AutterError` or `if let`.
- Add a regression test: inject a panic in a hook and assert the underlying `git` still runs and
  exits `0`.

**Effort.** S–M. **Depends on.** none.

---

## C3 — Close attribution data-gaps (`overrode`, line-stats, known-human rebase)  ⭐ foundational

**What & why.** Three open TODOs in core silently lose or fail to persist attribution detail that
the best analytics features (survival, churn, defect rate) need:
1. `src/authorship/virtual_attribution.rs:1215` — known-human attribution is **not propagated
   through the rebase path** (`humans: BTreeMap::new()`), so known-human marks can be dropped on rebase.
2. `src/authorship/authorship_log_serialization.rs:472` — the note format does **not store
   overridden state** for line ranges (the `LineAttribution.overrode` field in
   `src/authorship/attribution_tracker.rs:39` is computed but not round-tripped to the note).
3. `src/authorship/authorship_log_serialization.rs:506,513` — checkpoint kind and `LineStats`
   are approximated/empty in the note (`CheckpointKind::AiAgent` hardcoded; line-stats `TODO`).

**Builds on.** `AuthorshipLog`/`AttestationEntry` (`src/authorship/authorship_log.rs`),
`LineAttribution.overrode`, `Checkpoint.line_stats` (`CheckpointLineStats`),
the v3 serializer (`authorship_log_serialization.rs`), `RebaseNoteCache`
(`src/authorship/rebase_authorship.rs:25`).

**CLI changes.**
- Extend the v3 note metadata (in a **backward-compatible** way — add optional fields, keep
  `schema_version: authorship/3.0.0` parseable by old readers, or bump to `3.1.0` with a
  migration) to persist: per-range `overrode` (prior author_id) and per-attestation
  `line_stats`/`checkpoint_kind`.
- Wire `humans`/`sessions` through the rebase rewrite path so known-human marks survive history
  rewrites (Task 12 in the TODO).
- Update the spec doc `specs/autter_standard_v3.0.0.md` to describe the new optional fields.

**Why it's a prerequisite.** "AI line later rewritten by a human" (survival/churn, P-A3) and
"reverted AI line" (P-Q1) both require the `overrode`/kind data to be durable in the note. Without
C3, those features can only approximate from the local working log, which doesn't survive clone.

**Effort.** M. **Depends on.** none, but **unblocks** P-A2, P-A3, P-A4, P-Q1.

---

## C4 — Unified argument & global-flag parsing

**What & why.** Every handler hand-rolls a `while i < args.len()` loop (see `handle_checkpoint`,
`handle_status`). This is why flags are inconsistent and undiscoverable: `--json` exists on ~13
commands, `--plain` on 6, `--quiet` on 1, `--no-color` on 2. There's no per-subcommand `--help`.

**Builds on.** `src/commands/autter_handlers.rs` dispatch and the monolithic help block.

**CLI changes.**
- Introduce a thin shared parser for the **direct path only** (leave the proxy `argv` dispatch
  untouched — it must pass unknown flags straight to git). Either a small internal helper or adopt
  `clap` scoped to `autter` subcommands.
- Standardize global flags across all subcommands: `--json`, `--quiet`, `--no-color`,
  `-C <path>` (run as if in `<path>`), `-h/--help`.
- Give each subcommand its own `--help` text instead of one giant block.

**Effort.** M. **Depends on.** none. **Enables.** C5, C6.

---

## C5 — Universal, stable `--json` output

**What & why.** `status` and `blame` already emit structured output (`StatusOutput` in
`src/commands/status.rs`; `JsonBlameOutput` in `src/commands/blame.rs:1273`). Automation and the
IDE extensions need a consistent machine-readable contract across **all** read commands.

**Builds on.** Existing `#[derive(Serialize)]` output structs; `serde_json`.

**CLI changes.**
- Add `--json` (via C4's global handling) to `whoami`, `log`, `stats`, `diff`, `fetch-notes`,
  `config`, `doctor` (C7), `report` (C13).
- Define stable, versioned JSON schemas (include a `"schema"` field per command) so downstream
  consumers can rely on them; document in `specs/`.
- Ensure JSON mode writes **only** JSON to stdout (diagnostics to stderr) so piping works.

**Effort.** M. **Depends on.** C4 (nice to have). **Consumers.** IDE extensions, CI, P-R9.

---

## C6 — `NO_COLOR` / TTY-aware coloring

**What & why.** Color/`--plain` handling is ad hoc. Honor the standard `NO_COLOR` env var and
auto-disable color when stdout is not a TTY, project-wide.

**Builds on.** Existing terminal-detection `#[cfg]` code and the `--plain`/`--no-color` flags.

**CLI changes.**
- Centralize a `should_colorize()` helper: `false` if `NO_COLOR` set, `--no-color`/`--plain`
  passed, or stdout is not a TTY; honor it in all human-readable output paths.

**Effort.** S. **Depends on.** C4 (for flag plumbing).

---

## C7 — `autter doctor` diagnostics  ⭐ quick win

**What & why.** A single command that tells a user (and support) exactly why something isn't
working. Directly attacks the class of bug behind the v1.6.3 "Prompt was not saved" issue — today
there's no way to see that the sync queue is stuck.

**Builds on.** Daemon control API `status.family` (`src/daemon/control_api.rs`); `whoami`/auth
(`src/commands/whoami.rs`, `src/auth/`); org resolution (`resolve_org_for_repo_cached`,
`src/api/client.rs`); local queues in `InternalDatabase` (`cas_sync_queue`), `NotesDatabase`,
`FileChangesDatabase`; hook install detection (`src/commands/install_hooks.rs`).

**CLI changes.** New `src/commands/doctor.rs`, dispatched from `autter_handlers.rs`. Checks:
- Hooks installed in each detected tool/IDE; git proxy active.
- Daemon running + reachable over the control socket; last flush timestamp.
- Login/token validity and expiry (`StoredCredentials`); org resolved for the current repo.
- **Pending sync queue depths**: `cas_sync_queue`, notes queue, file-changes queue.
- `prompt_storage` mode and `api_base_url`/`notes_backend` config in effect.
- Output a human checklist (✓/✗ with remediation hints) and `--json` (C5).

**Effort.** M. **Depends on.** C5 for JSON. **Pairs with.** C8.

---

## C8 — Sync-state surfaced in `autter status`  ⭐ quick win

**What & why.** Turn a silent failure mode into a visible one. Add a one-line sync summary to
`autter status`: e.g. `sync: 0 prompts / 2 notes pending, last flush 8s ago`.

**Builds on.** The same queue counts as C7; `StatusOutput` struct in `src/commands/status.rs`.

**CLI changes.**
- Extend `StatusOutput` with a `sync` section (queue depths + last-flush age) and render it in
  both human and `--json` modes.
- Read counts from the daemon via `control_api` when available; fall back to reading the local
  SQLite queues directly when the daemon is down.

**Effort.** S. **Depends on.** C7 (shared queue-count helper).

---

## C9 — `autter sync --backfill`

**What & why.** Repos onboarded late have local notes that never reached the cloud
(`authorship_notes`), and historical agent-authored commits may have no notes at all. Give users a
way to recover.

**Builds on.** Bulk note tooling: `src/commands/notes_migrate.rs`, `fetch-notes`,
`push-authorship-notes`; `simulate_agent_authorship` (`src/authorship/agent_detection.rs:101`)
for synthesizing attribution from recognized agent emails/usernames.

**CLI changes.**
- New flag/subcommand `autter sync --backfill [--since <ref>] [--dry-run]`: walk history, find
  commits whose local note isn't yet uploaded, enqueue them; optionally synthesize attribution for
  recognized agent commits lacking notes.
- Print a clear summary and **explicitly warn** that pre-instrumentation commits are permanently
  linkless (no transcript backfill possible).

**Effort.** M. **Depends on.** none (works with current upload path; richer with SI-2/Rail A).

---

## C10 — Batch git subprocess calls in hot paths

**What & why.** The rebase path already learned this: `RebaseNoteCache`
(`src/authorship/rebase_authorship.rs:25`) pre-loads **all** notes in one batch instead of
per-commit `git notes show`. The same per-line / per-commit shell-out pattern still exists
elsewhere and is real latency on large files (the codebase explicitly flags "large source files").

**Builds on.** `RebaseNoteCache` as the reference pattern; `Repository` exec helpers
(`src/git/repository.rs`).

**CLI changes.**
- Profile `src/commands/blame.rs` (`overlay_ai_authorship`, the per-hunk note loads) and the
  checkpoint diffing path; replace per-commit/per-line `git` invocations with batched reads
  (`git notes ... --batch`, single `git cat-file --batch`, etc.).
- Add a perf regression bench under `benches/` for blame on a large file.

**Effort.** M. **Depends on.** none. **Related.** `specs/runaway-memory-plan.md`.

---

## C11 — Proxy fast-path for hookless commands

**What & why.** Every `git` invocation pays the proxy's pre/post-hook setup. Read-only commands
with no AI relevance (`git status`, `git log`, `git diff`, `git rev-parse`, completion queries)
should take the cheapest possible path and never spin up checkpoint/attribution machinery.

**Builds on.** `commands::git_handlers::handle_git` subcommand dispatch and the per-subcommand
pre/post hooks in `src/commands/hooks/`.

**CLI changes.**
- Maintain an allowlist of "passthrough" subcommands that skip hook setup entirely and `exec` git
  immediately (still forwarding signals on Unix).
- Add a micro-bench asserting passthrough overhead is within a small budget vs. bare `git`.

**Effort.** S–M. **Depends on.** none. **Risk.** medium — must be sure a "hookless" command
truly never needs a post-hook (e.g. some `git diff` invocations feed checkpoint state); validate
against `wrapper.pre_state`/`wrapper.post_state` usage.

---

## C12 — `autter blame --why <file>:<line>`  ⭐ quick win

**What & why.** Jump from a line straight to the prompt/PR/issue that produced it.

**Builds on.** `AutterBlameOptions` (`src/commands/blame.rs:77`), `BlameAnalysisResult`/
`JsonBlameOutput` (per-line prompt hash + `PromptRecord`), local prompt resolution
(`resolve_cas_messages`, `src/authorship/transcript.rs:154`), `open_browser`
(`src/commands/personal_dashboard.rs`).

**CLI changes.**
- Add `--why` to `AutterBlameOptions`. For a target line: resolve prompt hash → print the prompt
  transcript locally (from `cas_cache`/queue) and/or open
  `app.autter.dev/.../prompt/<hash>` (or the PR thread) via `open_browser`.
- Issue links come from `PromptRecord.custom_attributes["issue"]` when present (populated by P-C2).

**Effort.** S. **Depends on.** none for the local view; P-C2 for issue links.

---

## C13 — `autter report --since <ref>`  ⭐ quick win

**What & why.** Aggregate attestations across a commit range into a per-module AI% report —
useful for release notes, standups, and the compliance story. Pure local read; no backend.

**Builds on.** Authorship notes across a range (`refs/notes/ai`), the stats helpers
(`src/authorship/stats.rs`, `stats_from_authorship_log`), `LineRange`/`FileAttestation`.

**CLI changes.**
- New `src/commands/report.rs`: walk commits in `<ref>..HEAD`, load each note, roll up
  `{ai_lines, known_human_lines, untracked_lines}` per file/directory and per agent/model.
- Output a Markdown table by default and `--json` (C5). Optionally `--by agent|model|dir|author`.
- (Optional, becomes a platform feature P-G8) `--share` to POST the report and get a URL via the
  bundle pattern.

**Effort.** S–M. **Depends on.** none (richer with C3's durable line-stats).

---

## Suggested CLI-only delivery order

1. **C1 + C2** — reliability floor (correct exit codes, panic-safe proxy).
2. **C7 + C8** — self-diagnostics + visible sync health (kills the #1 support issue).
3. **C4 + C5 + C6** — DX consistency (unified parsing, universal JSON, color hygiene).
4. **C3** — close the data-gaps (also unblocks the best platform analytics).
5. **C12 + C13** — user-facing quick wins.
6. **C10 + C11 + C9** — performance + recovery tooling.
