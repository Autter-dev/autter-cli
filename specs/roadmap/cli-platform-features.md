# CLI + Platform Features

Features that span the **CLI** and the **platform** — the org Postgres data plane (Rail A), the
`/worker/*` control plane (Rail B), and the web app (Rail C). See
[`README.md`](./README.md#the-three-integration-rails) for the rail definitions and
[`cli-only-features.md`](./cli-only-features.md) for standalone CLI work.

> Many features here only need data to *land* in the org Postgres; the rendering is pure web-app
> work. Those say "Platform changes: app-side only." The expensive ones need a new table/column
> (Rail A) or a new interactive endpoint (Rail B).

---

## Shared infrastructure

Build these primitives first; they are referenced by the feature specs below.

### SI-1 — Note parser in the web app
Port the `authorship/3.0.0` text format (the serializer in
`src/authorship/authorship_log_serialization.rs`; spec in `specs/autter_standard_v3.0.0.md`) into
the web app so it can read `authorship_notes.content` and intersect attestation `line_ranges` with
PR diff hunks. **Unlocks** P-R1, P-R2, P-R3, P-A1, P-G5, P-G8.

### SI-2 — Policy / config pull loop
The structural gap: today config is local-only. Add:
- **Endpoint** `POST /worker/policy` (Rail B) returning org policy JSON, e.g.
  `{ "require_known_human_on_ai": true, "block_untracked_merge": false, "min_review_coverage": 0.8,
  "sensitive_paths": ["crypto/**","payments/**"], "module_quotas": {"payments/**": 0.2} }`.
- **CLI**: the daemon (`src/daemon/telemetry_worker.rs` loop) fetches policy on an interval and
  writes `~/.autter/policy.json`; `Config` (`src/config.rs`) exposes a `policy()` accessor read via
  `Config::fresh()`. Enforcement points (commit/push hooks, `check-policy`) read it.
**Unlocks** P-R4, P-R7, P-G1–G4, P-C3.

### SI-3 — Interactive `/worker/*` request/response channel
Today the only request/response cloud calls are OAuth + `/api/bundles`. Add a generic
authenticated POST helper usage for synchronous endpoints (review, query) built on
`ApiContext::post_json` (`src/api/client.rs`), Bearer/`X-API-Key` auth already handled.
**Unlocks** P-R6, P-R8, P-E1.

### SI-4 — New metric event IDs + extended `Committed` values
`MetricEvent` is `{t,e,v,a}` and `MetricEventId` currently has `Committed=1, AgentUsage=2,
InstallHooks=3, Checkpoint=4, SessionEvent=5, OtelTrace=6` (`src/metrics/types.rs:18`). Add:
- `LineOverride=7` — emitted when a `KnownHuman` checkpoint overrides an AI line
  (hook in `AttributionTracker::update_attributions_for_checkpoint`, using `LineAttribution.overrode`).
- `RevertedLine=8` — emitted from the revert/reset path (`src/git/rewrite_log.rs` events).
- `UnsupervisedMerge=9` — emitted when the merge gate (P-R4) trips.
- Extend the `Committed=1` event's `v` map with `{ai_lines, known_human_lines, untracked_lines}`
  computed from the freshly written note in `src/authorship/post_commit.rs`.
All flow through the existing metrics queue → `cli_metrics` (Rail A). **Unlocks** P-R5, P-A1–A5,
P-A7, P-Q1, P-O3.

### SI-5 — Activate `cli_audit_log`
The table already exists in the org schema (`src/api/org_db.rs:99-157`) but is under-used. Start
writing rows on note upsert and on every policy/gate decision. **Unlocks** P-G5, P-G6, P-G7.

### SI-6 — `/worker/repos/register` + chained onboarding
New endpoint so a repo appears in the dashboard before its first note upserts; `autter onboard`
calls it after login + org resolution. **Unlocks** P-O1, P-O2, P-O3.

### SI-7 — Provenance read API + webhooks
A thin authenticated read layer over the org Postgres + an event emitter. **Unlocks** P-E1, P-C3, P-C5.

---

## Theme: Review & PR intelligence

### P-R1 — Provenance-weighted review
**What & why.** The reviewer scrutinizes AI-authored hunks harder and lightly skims verified-human
lines, focusing bug-finding where the risk is.
**Builds on.** Synced `authorship_notes`; `PromptRecord`/`SessionRecord` carrying `AgentId{tool,id,model}`.
**CLI changes.** none (data already synced).
**Platform changes.** SI-1 parser → per-hunk `ai_fraction` + `tool/model`; feed into the reviewer's
severity weighting/prompt.
**Effort.** M. **Depends on.** SI-1.

### P-R2 — Per-line agent badges in the PR
**What & why.** Render the PR diff with inline "🤖 Claude / Cursor / human / untracked" gutter marks.
**Builds on.** `JsonBlameOutput` (`src/commands/blame.rs:1273`): `lines: {"start-end" → prompt_hash}`
+ `prompts: {hash → record}`.
**CLI changes.** Optionally extend `JsonBlameOutput` to surface `tool`/`model` explicitly (today
behind the prompt hash).
**Platform changes.** Map `AgentId.tool` → label/icon; overlay on the diff (app-side, from the note).
**Effort.** S–M. **Depends on.** SI-1.

### P-R3 — Prompt-in-review context
**What & why.** Each AI hunk links to the exact prompt that produced it; reviewers see intent vs. output.
**Builds on.** `PromptRecord.messages_url = "cas:<hash>"` → `cas_objects` (Rail A);
`Message` enum (`src/authorship/transcript.rs`).
**CLI changes.** none (already uploads transcripts; respect `prompt_storage=local` → null `messages_url`).
**Platform changes.** App reads `cas_objects.content` by hash, renders the transcript beside the hunk;
degrade gracefully when null.
**Effort.** M. **Depends on.** SI-1.

### P-R4 — Unsupervised-AI merge gate  ⭐
**What & why.** Block/flag AI hunks that **no human ever reviewed** — the single most defensible
product claim.
**Builds on.** `CheckpointKind::{AiAgent, KnownHuman, Human}` (`src/authorship/working_log.rs:49`);
`LineAttribution.overrode`. "AI line with no overriding `KnownHuman` attribution" is directly computable.
**CLI changes.** New `src/commands/check_policy.rs`: given staged tree or commit range, flag
unsupervised AI ranges. Wire as a **pre-push hook** (`src/commands/hooks/push_hooks.rs`) and a
**CI gate** (`src/ci/github.rs`/`gitlab.rs` `run` path → non-zero exit + PR status check). Emit
`UnsupervisedMerge=9` (SI-4).
**Platform changes.** Thresholds via SI-2 policy; app shows gate hits.
**Effort.** M. **Depends on.** SI-2, SI-4.

### P-R5 — Untracked-changes flagging
**What & why.** Surface "provenance unknown" (legacy `Human`/untracked) ranges so teams tighten setup.
**Builds on.** `CheckpointLineStats`; untracked ranges in notes; `file_change_counts`.
**CLI changes.** Add `unattributed_lines` to the `Checkpoint`/`Committed` metric (SI-4).
**Platform changes.** "Provenance coverage" tile = (ai + known_human)/total.
**Effort.** S. **Depends on.** SI-4.

### P-R6 — `autter review` (local pre-push review)  ⭐
**What & why.** Run the cloud reviewer against the working diff from the terminal, before pushing.
**Builds on.** Bundle path: `create_bundle` POSTs `{prompts, files}` to `/api/bundles`
(`src/api/bundle.rs:24`); diff/bundle builder in `src/commands/diff.rs:2155`.
**CLI changes.** New `src/commands/review.rs` (dispatched in `autter_handlers.rs`): build
working-tree diff + attribution context (reuse bundle builder), POST to new **`/worker/review`**
(SI-3), stream findings to the terminal; `--json` (C5).
**Platform changes.** `/worker/review` endpoint that runs the reviewer using the attribution
context (ties to P-R1); request/response structs in `src/api/types.rs`.
**Effort.** L. **Depends on.** SI-3.

### P-R7 — `autter rules` (pull org rules, local lint)
**What & why.** Pull the org's plain-English rules and evaluate the working tree before the PR.
**Builds on.** SI-2 endpoint family; `ApiContext` HTTP.
**CLI changes.** `autter rules [pull|check]`: cache rules to `~/.autter/rules.json`; `check`
evaluates locally or delegates to the cloud (like P-R6).
**Platform changes.** `/worker/rules` returns the org rule set.
**Effort.** M. **Depends on.** SI-2 (or SI-3 for cloud eval).

### P-R8 — `autter fix`
**What & why.** Apply the reviewer's suggested patch from the terminal.
**Builds on.** P-R6/P-R7 responses carrying patches; the proxy already shells to git.
**CLI changes.** `autter fix <finding-id>`: fetch patch, `git apply`, then fire an AI checkpoint
(`mock_ai` equivalent) so the fix is itself attributed.
**Platform changes.** Review response includes machine-applicable patches.
**Effort.** M. **Depends on.** P-R6.

### P-R9 — IDE inline provenance + review comments
**What & why.** Show per-line agent badges + open review comments inside VS Code / JetBrains.
**Builds on.** Extensions already call `autter blame --json` + `show-prompt`; `cas_cache`.
**CLI changes.** `autter comments --file <path> [--json]` reading the app's review-comments table
(Rail A read) or via SI-7.
**Platform changes.** Expose review comments per file.
**Effort.** M. **Depends on.** C5, P-R2; SI-7 (optional).

### P-R10 — PR Merge-Confidence Score  ⭐
**What & why.** One 0–100 number on every PR from signals only Autter has: AI fraction, author
historical survival (P-A3), sensitive-path touches, unsupervised flag (P-R4).
**Builds on.** `authorship_notes` ⨝ `cli_metrics`.
**CLI changes.** none.
**Platform changes.** Scoring service (app-side) → GitHub/GitLab status check.
**Effort.** M. **Depends on.** SI-1, SI-4, P-A3.

### P-R11 — Prompt-intent drift detection
**What & why.** Flag when the AI did more/less than the prompt asked (scope creep, deleted tests).
**Builds on.** Prompt in `cas_objects`; the diff.
**CLI changes.** none.
**Platform changes.** App-side LLM judge comparing prompt vs. diff at review time; feeds P-R10.
**Effort.** M. **Depends on.** SI-1.

---

## Theme: Analytics & dashboards

### P-A1 — AI-authorship dashboard
**What & why.** % of merged code that is AI vs known-human vs untracked, by repo/team/author/time.
**Builds on.** `cli_metrics` (`Committed`, `Checkpoint`, `AgentUsage`) + `file_change_counts`,
uploaded every 3s (`src/daemon/telemetry_worker.rs`).
**CLI changes.** Extend `Committed` with exact per-commit line counts (SI-4).
**Platform changes.** App aggregation/tiles over `cli_metrics`; slice by `uploaded_by`/`distinct_id`.
**Effort.** M. **Depends on.** SI-4.

### P-A2 — Per-agent/model leaderboard
**What & why.** Which agent/model writes code that *survives* (isn't rewritten/reverted).
**Builds on.** `AgentId{tool,model}` on every line; `LineOverride`/`RevertedLine` (SI-4); rewrite
tracking keeps attribution across rebases (`src/authorship/rebase_authorship.rs`).
**CLI changes.** emit SI-4 events.
**Platform changes.** App cohorts `cli_metrics` by `tool`/`model`.
**Effort.** M. **Depends on.** C3, SI-4.

### P-A3 — AI code survival / churn  ⭐
**What & why.** "X% of AI lines untouched after 30 days; model Y rewritten 3× more often." A metric
only this dataset can produce.
**Builds on.** `LineAttribution.overrode` (durable via C3); `LineOverride` events.
**CLI changes.** C3 + SI-4 `LineOverride`.
**Platform changes.** Survival curves per agent/model over `cli_metrics`.
**Effort.** M. **Depends on.** C3, SI-4.

### P-A4 — Prompt-quality analytics
**What & why.** Correlate prompt characteristics with downstream churn/defects; coach prompting.
**Builds on.** Transcripts in `cas_objects` (with `tool/model/session_id`); churn from P-A3.
**CLI changes.** none.
**Platform changes.** App batch job joins `prompt_hash` → transcript → survival.
**Effort.** M. **Depends on.** P-A3.

### P-A5 — Cost / ROI attribution
**What & why.** "$ per durable line of code" per team/model.
**Builds on.** `model` in transcript; many transcripts include token usage in events parsed by
`parse_transcript_events` (`src/authorship/transcript.rs`); `PromptRecord` additions/deletions.
**CLI changes.** Extract `usage` tokens into CAS metadata (or a `TokenUsage` metric) in transcript parsing.
**Platform changes.** Price table × tokens; combine with P-A3 survival.
**Effort.** M. **Depends on.** SI-4 (or CAS metadata), P-A3.

### P-A6 — Enriched standups / sprint reports
**What & why.** Inject "% AI-authored, top agent, N unsupervised merges" into existing app reports.
**Builds on.** the analytics tables above.
**CLI changes.** none.
**Platform changes.** App templating.
**Effort.** S. **Depends on.** P-A1.

### P-A7 — Model A/B & migration impact
**What & why.** When a team switches agents/models, compare survival/churn/defect before vs. after.
**Builds on.** `AgentId{tool,model}` time series in `cli_metrics`.
**CLI changes.** none.
**Platform changes.** App cohorting by model over time.
**Effort.** M. **Depends on.** P-A3, P-Q1.

### P-A8 — Anonymized industry benchmark
**What & why.** "Your org is 38% AI — p75 in your cohort; survival above median."
**Builds on.** cross-org aggregates (opt-in).
**CLI changes.** none.
**Platform changes.** Cross-org aggregation service with privacy guarantees.
**Effort.** M. **Depends on.** P-A1, P-A3.

### P-A9 — BI / warehouse export (dbt models)
**What & why.** Canonical dbt models / semantic layer over `authorship_notes`, `cli_metrics`,
`file_change_counts` so data teams build their own dashboards.
**Builds on.** Rail A tables.
**CLI changes.** none.
**Platform changes.** Published dbt package + docs.
**Effort.** M. **Depends on.** SI-4 (richer events).

---

## Theme: Governance & compliance

### P-G1 — Policy push & enforcement
**What & why.** Org defines policy; CLI enforces it as a pre-commit/pre-push gate.
**Builds on.** SI-2; commit/push hooks; `check-policy` (P-R4).
**Effort.** M–L. **Depends on.** SI-2, P-R4.

### P-G2 — AI budget / quota governance
**What & why.** "Module `payments/` may be ≤20% AI"; dashboard the burn-down.
**Builds on.** SI-2 `module_quotas`; path-glob matcher in push hook; `file_change_counts`.
**CLI changes.** quota check in `check_policy.rs`.
**Platform changes.** quota UI + burn-down tiles.
**Effort.** M. **Depends on.** SI-2, P-R4.

### P-G3 — AI code review SLA
**What & why.** Guarantee every AI hunk gets human eyes within N days; nudge owners.
**Builds on.** `LineOverride`/`KnownHuman`-touched-AI timing (SI-4).
**CLI changes.** emit the override-timing event.
**Platform changes.** SLA dashboard + Slack nudges.
**Effort.** M. **Depends on.** SI-4, P-A3.

### P-G4 — License / IP risk surfacing
**What & why.** AI code in sensitive paths requires human sign-off.
**Builds on.** SI-2 `sensitive_paths`; per-file AI ranges in notes.
**CLI changes.** path matcher in `check_policy.rs`.
**Platform changes.** risk panel.
**Effort.** S–M. **Depends on.** P-R4, SI-2.

### P-G5 — Immutable AI-provenance audit log
**What & why.** Queryable, exportable system of record for "which code was AI-generated."
**Builds on.** `cli_audit_log` (already in schema), immutable `authorship_notes` keyed by `commit_sha`.
**CLI changes.** write audit rows on note upsert + gate decisions (SI-5).
**Platform changes.** audit query/export UI over `authorship_notes ⨝ cli_audit_log`.
**Effort.** M. **Depends on.** SI-5.

### P-G6 — Signed attestations
**What & why.** Tamper-evident provenance; answers "can you trust AI-reviewing-AI."
**Builds on.** notes under `refs/notes/ai`; CI already pushes these refs.
**CLI changes.** sign note content at `post_commit.rs` with a per-machine/org key; store signature
in note metadata + a `signature` column on `authorship_notes`.
**Platform changes.** verify signatures; show trust badge.
**Effort.** L (key management). **Depends on.** SI-5.

### P-G7 — Compliance evidence pack export  ⭐
**What & why.** One-click signed export: every AI line in a release, the agent/model, the prompt,
and whether a human reviewed it. The enterprise wedge (SOC2 / EU AI Act / due diligence).
**Builds on.** `authorship_notes`, `cas_objects`, `cli_audit_log`.
**CLI changes.** `autter report --since <tag> --evidence` produces the structured pack (extends C13).
**Platform changes.** exportable report builder + shareable URL.
**Effort.** M. **Depends on.** SI-5, C13.

### P-G8 — Release provenance report (shareable)
**What & why.** "This release is X% AI across these modules, by these agents."
**Builds on.** C13 local report + bundle-style share.
**CLI changes.** `autter report --since <tag> --share` → POST → URL.
**Platform changes.** render shared report page.
**Effort.** S–M. **Depends on.** C13, SI-1.

---

## Theme: Quality & incident intelligence

### P-Q1 — Revert/incident → AI defect rate  ⭐ highest "nobody-else-has-this"
**What & why.** Detect reverts/incident-linked commits, trace the reverted lines back through
attribution, and compute **defect rate per model**. Directly rebuts "AI ships bugs" — with ground truth.
**Builds on.** `RewriteLogEvent::{RevertMixed, Reset, ...}` (`src/git/rewrite_log.rs`); re-attribution
of reverted ranges; `AgentId{tool,model}` + prompt hash on those lines.
**CLI changes.** In the revert/reset post-hook, re-attribute the reverted ranges and emit
`RevertedLine=8` (SI-4) with `{tool, model, prompt_hash, repo, file}`.
**Platform changes.** "defect rate by agent/model" dashboard with drill-down to the offending prompt.
**Effort.** M. **Depends on.** C3, SI-4.

---

## Theme: Knowledge & search

### P-K1 — Org prompt library / search  ⭐
**What & why.** Index every prompt that produced shipped, surviving code into a searchable library:
"prompts that successfully added a migration / wrote a React form." Reuse winning prompts; onboard juniors.
**Builds on.** `cas_objects` transcripts with `{tool,model,session_id}`; survival from P-A3.
**CLI changes.** opt-in flag (respect `prompt_storage=local`).
**Platform changes.** embed transcripts → vector search UI; rank by survival.
**Effort.** M–L. **Depends on.** SI-1 (link prompts↔lines), P-A3 (survival ranking).

### P-K2 — "Who understands this AI code" ownership map
**What & why.** AI code has no natural owner — a bus-factor risk. Heatmap "AI-authored & never
human-touched" per file.
**Builds on.** AI ranges + `KnownHuman` checkpoint history.
**Platform changes.** CodeGraph overlay colored by orphaned AI code.
**Effort.** M. **Depends on.** SI-1, SI-4.

### P-K3 — Auto-docs & PR descriptions from prompts
**What & why.** Generate PR summaries / code comments / changelog entries from the captured *intent*
(the prompt), higher-fidelity than diff-only.
**Builds on.** transcripts in `cas_objects`; the app reviewer already writes PR summaries.
**Platform changes.** feed prompts for AI hunks into the summary generator.
**Effort.** M. **Depends on.** SI-1.

### P-K4 — AI session replay
**What & why.** Reconstruct an AI coding session as a timeline: "step 1 read these files, step 2
wrote this function, step 3 the human rewrote half."
**Builds on.** ordered `checkpoints.jsonl` (`timestamp` + `trace_id`) + the transcript.
**CLI changes.** `autter session <trace_id> --json` exporter (local).
**Platform changes.** timeline viewer.
**Effort.** M. **Depends on.** C5; SI-1 for cloud view.

---

## Theme: Collaboration & real-time

### P-C1 — Slack / Notion provenance digests
Daily "AI authorship + unsupervised merges" digest via existing integrations. App-side over
`cli_metrics`. **Effort.** S. **Depends on.** P-A1.

### P-C2 — Jira / Linear back-link
Populate `PromptRecord.custom_attributes["issue"]` in checkpoint presets
(`src/commands/checkpoint_agent/presets/`) from branch name/transcript; surface in blame JSON; app
joins to the issue ("how much of this ticket was AI-built"). **Effort.** M. **Depends on.** none.

### P-C3 — Real-time merge / policy alerts
When the gate trips (P-R4), POST to `/worker/alerts` (SI-7) → fan out to Slack/Linear. **Effort.** M.
**Depends on.** P-R4, SI-7.

### P-C4 — Org "AI Pulse" live feed
Lightweight `commit-attributed` event (daemon flushes every 3s) → live dashboard tile (current AI%,
top agent now, unsupervised alerts). **Effort.** M. **Depends on.** SI-4.

### P-C5 — Slack "ask about this code" bot
`@autter who wrote payments/charge.ts:42` → agent/model/prompt/survival, from the same blame/CAS
data. **Effort.** M. **Depends on.** SI-7.

---

## Theme: Onboarding & identity

### P-O1 — One-command team onboarding
Chain `autter onboard`: login (`run_device_login`) → resolve/confirm org for `origin`
(`resolve_org_for_repo_cached`) → install hooks → register repo (`/worker/repos/register`, SI-6).
**Effort.** M. **Depends on.** SI-6.

### P-O2 — Seat / identity reconciliation
App-side `identity_map(git_email → app_user_id)`; CLI already sends `X-Author-Identity` +
`distinct_id`. Add `git_email`/`committer` to metric emission for auto-suggested mappings.
**Effort.** M. **Depends on.** SI-6.

### P-O3 — Web-based agent health page
Periodic heartbeat metric `{repo, machine, last_sync_ts, queue_depths, cli_version}` → app "agent
health" page (which repos/machines report, where the gaps are). Mirrors `autter doctor` (C7) to the
web. **Effort.** S–M. **Depends on.** SI-4, C7.

---

## Theme: Ecosystem

### P-E1 — Provenance Query API + webhooks
Authenticated read API ("GET all AI lines by model X between dates") + webhooks
(`commit.attributed`, `unsupervised.merged`, `ai.reverted`) so customers wire Autter into their own
BI/security tooling. Thin layer over org Postgres. **Effort.** M–L. **Depends on.** SI-7.

---

## Theme: Plumbing

### P-PL1 — Robust offline→online sync hardening
Harden the durable queues (`cas_sync_queue`, notes queue, file-changes queue) and their drains in
`src/daemon/telemetry_worker.rs` (already 20×50/tick with 10-min stale-lock recovery). Add
queue-depth observability surfaced via `control_api` `status.family` → consumed by `autter doctor`
(C7) and the agent health page (P-O3). This is the class of bug behind v1.6.3's "prompt not saved."
**Effort.** S–M. **Depends on.** C7/C8 for surfacing.

---

## Cross-cutting privacy note

`prompt_storage` has three modes (`default` → CAS to org Postgres, `notes` → in git notes after
redaction, `local` → never leaves the machine). Every feature that reads prompts (P-R3, P-K1, P-K3,
P-A4, P-C5) **must** degrade gracefully when `messages_url` is null because the user chose `local`.
Cross-org features (P-A8) must be opt-in and aggregate-only.
