# Autter Roadmap — Feature Specifications

This directory is the master plan for the feature work that makes both the **Autter CLI**
and the **Autter platform** (`autter.dev` / `app.autter.dev` / the org Postgres data plane)
dramatically better. Every feature here is grounded in the data the CLI already captures:
**line-level AI/human provenance, the prompts that produced each line, override/survival
history, and history-rewrite tracking** — synced into a per-org Postgres that the web app reads.

The strategic insight behind the whole roadmap: the CLI is the only component in the market
with **verified, line-level, prompt-linked provenance of every commit**. Almost every feature
below is a way to turn that signal into a product surface nobody else can build.

## How this is organized

| File | Scope |
|------|-------|
| [`cli-only-features.md`](./cli-only-features.md) | Features that live entirely in the CLI. No backend changes. Ship independently. |
| [`cli-platform-features.md`](./cli-platform-features.md) | Features that span the CLI **and** the platform (data plane + web app). |
| `README.md` (this file) | Index, priority matrix, and the shared infrastructure every platform feature depends on. |

Each feature entry follows the same template:

- **What & why** — one paragraph.
- **Builds on** — the existing structs/files/tables it reuses (with real paths).
- **CLI changes** — concrete code to add/modify.
- **Platform changes** — Postgres + web-app work (platform features only).
- **Data flow** — end to end.
- **Effort** — S / M / L.
- **Depends on** — prerequisite features.

---

## The three integration "rails"

Every platform feature rides one of three existing seams. Understanding them avoids re-explaining
the plumbing in each spec.

- **Rail A — Org Postgres (data plane).** The CLI connects directly to the org's Postgres using
  the `org_db_url` claim decoded from the JWT (`src/api/org_db.rs:54`, `identity_from_token`).
  Tables today: `authorship_notes`, `cas_objects`, `cli_metrics`, `file_change_counts`,
  `cli_audit_log` (schema provisioned at `src/api/org_db.rs:99-157`). **Adding a feature's data =
  add a table/column + an `upsert_*`/`read_*` fn here + a flush step in the daemon loop
  (`src/daemon/telemetry_worker.rs:286-323`).**
- **Rail B — Control plane (`/worker/*` HTTP on `api.autter.dev`).** Today only OAuth
  (`src/auth/client.rs` → `/worker/oauth/*`), `resolve-org`, and `/api/bundles`
  (`src/api/bundle.rs`) use HTTP. New **interactive** CLI→cloud calls (review, rules pull,
  policy pull, query) go here via `ApiContext::post_json` (`src/api/client.rs`).
- **Rail C — The web app reads the same Postgres** and renders dashboards / PR overlays.
  Pure-visualization features need no CLI change beyond making sure the data lands in Rail A.

## The two structural gaps to close first

The current codebase is a mostly **one-way exporter**. Two gaps, once closed, unlock most of the roadmap:

1. **No remote policy/config pull.** All config is local `~/.autter/config.json` read at process
   start (`Config::fresh()` re-reads in daemon mode). → Build **SI-2 (policy-pull loop)**.
2. **The only interactive cloud call is bundle/OAuth.** Everything else is fire-and-forget Postgres
   writes. → Build **SI-3 (request/response `/worker/*` channel)**.

---

## Shared infrastructure (build once, unlocks many)

These are not user-facing features; they are the platform primitives the features below assume.
Specced in detail at the top of [`cli-platform-features.md`](./cli-platform-features.md#shared-infrastructure).

| ID | Primitive | Unlocks |
|----|-----------|---------|
| **SI-1** | Note parser in the web app (port the `authorship/3.0.0` format) | P-R1, P-R2, P-R3, P-A1, P-G5, P-G8 |
| **SI-2** | Policy/config pull loop (`/worker/policy` → `~/.autter/policy.json`) | P-R4, P-R7, P-G1, P-G2, P-G3, P-G4, P-C3 |
| **SI-3** | Interactive `/worker/*` request/response channel | P-R6, P-R8, P-E1 |
| **SI-4** | New metric event IDs + extended `Committed` event values | P-R5, P-A1–A5, P-A7, P-Q1, P-O3 |
| **SI-5** | Activate the (already-present) `cli_audit_log` table | P-G5, P-G6, P-G7 |
| **SI-6** | `/worker/repos/register` + chained onboarding | P-O1, P-O2, P-O3 |
| **SI-7** | Provenance read API + webhooks (thin auth layer over org Postgres) | P-E1, P-C3, P-C5 |

---

## Priority matrix

"Leverage" = uniqueness of the data × breadth of features it unlocks. Build top-left first.

### Tier 0 — Foundations (do these before most platform work)
- **SI-2** Policy pull loop
- **SI-3** Interactive `/worker/*` channel
- **SI-4** New metric events (survival/override/revert signals)
- **C3** Close attribution data-gaps (`overrode` + line-stats + known-human rebase) — *prerequisite for all survival/churn analytics*

### Tier 1 — Highest leverage, "nobody else has this"
- **P-Q1** Revert/incident → AI defect rate
- **P-A3** AI code survival / churn
- **P-R4** Unsupervised-AI merge gate
- **P-R10** PR Merge-Confidence Score
- **P-K1** Org prompt library / search
- **P-G7** Compliance evidence pack

### Tier 2 — Strong, mostly app-side once foundations exist
- **P-A1** AI-authorship dashboard · **P-A2** agent leaderboard · **P-A7** model migration impact
- **P-R1/R2/R3** provenance-aware review · **P-R6** `autter review`
- **P-G1/G2/G3** governance/quota/SLA · **P-O1** onboarding
- **P-E1** provenance API + webhooks

### Tier 3 — High polish / differentiation
- **P-K2** ownership map · **P-K3** auto-docs · **P-K4** session replay
- **P-C1–C5** collaboration & real-time · **P-A8** benchmark · **P-A9/E** BI export
- **P-G6** signed attestations

### CLI-only quick wins (ship anytime, no backend)
- **C1** correct exit codes · **C2** panic-safe proxy · **C7** `autter doctor` · **C8** sync status
- **C5** universal `--json` · **C4** unified arg parsing · **C12** `blame --why` · **C13** `autter report`

---

## Feature index

### CLI-only ([details](./cli-only-features.md))
| ID | Feature | Effort |
|----|---------|--------|
| C1 | Correct exit codes for direct subcommands | S |
| C2 | Panic-safe git proxy (`catch_unwind`) | S–M |
| C3 | Close attribution data-gaps (`overrode`, line-stats, known-human rebase) | M |
| C4 | Unified argument & global-flag parsing | M |
| C5 | Universal, stable `--json` output | M |
| C6 | `NO_COLOR` / TTY-aware coloring | S |
| C7 | `autter doctor` diagnostics | M |
| C8 | Sync-state surfaced in `autter status` | S |
| C9 | `autter sync --backfill` | M |
| C10 | Batch git subprocess calls in hot paths | M |
| C11 | Proxy fast-path for hookless commands | S–M |
| C12 | `autter blame --why` | S |
| C13 | `autter report --since <ref>` | S–M |

### CLI + platform ([details](./cli-platform-features.md))
| ID | Feature | Theme | Effort |
|----|---------|-------|--------|
| P-R1 | Provenance-weighted review | Review | M |
| P-R2 | Per-line agent badges in PR | Review | S–M |
| P-R3 | Prompt-in-review context | Review | M |
| P-R4 | Unsupervised-AI merge gate | Review/Gov | M |
| P-R5 | Untracked-changes flagging | Review | S |
| P-R6 | `autter review` (local pre-push review) | Review | L |
| P-R7 | `autter rules` (pull org rules, local lint) | Review | M |
| P-R8 | `autter fix` | Review | M |
| P-R9 | IDE inline provenance + review comments | Review | M |
| P-R10 | PR Merge-Confidence Score | Review/Gov | M |
| P-R11 | Prompt-intent drift detection | Review | M |
| P-A1 | AI-authorship dashboard | Analytics | M |
| P-A2 | Per-agent/model leaderboard | Analytics | M |
| P-A3 | AI code survival / churn | Analytics | M |
| P-A4 | Prompt-quality analytics | Analytics | M |
| P-A5 | Cost / ROI attribution | Analytics | M |
| P-A6 | Enriched standups / sprint reports | Analytics | S |
| P-A7 | Model A/B & migration impact | Analytics | M |
| P-A8 | Anonymized industry benchmark | Analytics | M |
| P-A9 | BI / warehouse export (dbt models) | Analytics | M |
| P-G1 | Policy push & enforcement | Governance | M–L |
| P-G2 | AI budget / quota governance | Governance | M |
| P-G3 | AI code review SLA | Governance | M |
| P-G4 | License / IP risk surfacing | Governance | S–M |
| P-G5 | Immutable AI-provenance audit log | Compliance | M |
| P-G6 | Signed attestations | Compliance | L |
| P-G7 | Compliance evidence pack export | Compliance | M |
| P-G8 | Release provenance report (shareable) | Compliance | S–M |
| P-Q1 | Revert/incident → AI defect rate | Quality | M |
| P-K1 | Org prompt library / search | Knowledge | M–L |
| P-K2 | "Who understands this AI code" ownership map | Knowledge | M |
| P-K3 | Auto-docs & PR descriptions from prompts | Knowledge | M |
| P-K4 | AI session replay | Knowledge | M |
| P-C1 | Slack / Notion provenance digests | Collab | S |
| P-C2 | Jira / Linear back-link | Collab | M |
| P-C3 | Real-time merge / policy alerts | Collab | M |
| P-C4 | Org "AI Pulse" live feed | Collab | M |
| P-C5 | Slack "ask about this code" bot | Collab | M |
| P-O1 | One-command team onboarding | Onboarding | M |
| P-O2 | Seat / identity reconciliation | Onboarding | M |
| P-O3 | Web-based agent health page | Onboarding | S–M |
| P-E1 | Provenance Query API + webhooks | Ecosystem | M–L |
| P-PL1 | Robust offline→online sync hardening | Plumbing | S–M |

---

## Definition of "10x better"

- **For the CLI:** never breaks a user's `git`; never loses attribution; is fully scriptable
  (`--json` everywhere, correct exit codes); is self-diagnosing (`doctor`); and surfaces its own
  sync health so failures are visible, not silent.
- **For the platform:** turns one-way data export into a two-way product — provenance-aware
  review, governance you can enforce, analytics nobody else can produce (survival, defect rate
  by model), and a queryable provenance API that makes Autter the system of record for
  "what in our codebase is AI-generated, and can we trust it."
