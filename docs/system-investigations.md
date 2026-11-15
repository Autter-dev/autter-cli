# System questions, logs, and debugging history

Use the same private investigation history as Dashboard Ask:

```sh
autter login
autter ask "Why is checkout failing?" --repo checkout
autter ask "How can we verify the fix?" --thread <thread-id>
autter logs --repo checkout --severity error --service api --since 1h
autter logs --repo checkout --field attributes.http.request.method=POST --filter status_code:gte:500
autter threads search "connection pool"
autter threads show <thread-id>
autter lineage <issue-id>
```

Questions use captured Runtime evidence and source code to explain root causes, impact, and recommended fixes. Hypotheses and telemetry gaps should be explicit. Threads are saved automatically; you can resume them from Dashboard or CLI. Historical conclusions need fresh evidence before reuse.

Use `--org <slug>` for another organization you belong to and `--json` for complete structured output. `--mode codebase` and `--mode reviews` select the existing assistants. Resuming with `--thread` restores that thread's mode and repository unless you explicitly specify them; changing its scope requires a new thread.

Without `--repo`, a new question run inside a git repository is scoped to that repository when its default remote (`owner/name`) is connected to the organization; otherwise it goes out unscoped. Answers that point at Autter pages (a pull request, a file at a line, a scan finding, a Runtime issue) end with an **Open in Autter** list of absolute links, and links inside the answer are absolute too.

`autter dash` opens the page for where you are: the current repository's page when it is connected to the organization (`--org`, else your active one), else the organization dashboard, else (signed out) the app's dashboard. `--repo <owner/name|name>` picks the repository, `--no-open` prints the URL instead of opening it, and `--json` prints `{ url, target, organization, repository, repository_full_name, opened, fallback_reason }`, where `target` is `repo`, `org` or `home` and `fallback_reason` is null or one of `not_in_git_repo`, `no_remote`, `not_signed_in`, `repo_not_connected`, `lookup_failed`. The lookup waits at most 5 seconds and reads the repository list without starting an index refresh.

## From a Runtime issue to the code behind it

`autter lineage <issue-id>` prints the chain behind one Runtime issue (its id starts with `ri_`), top to bottom:

```sh
autter lineage ri_0123456789abcdef0123456789
autter lineage ri_0123456789abcdef0123456789 --repo shop --json
autter lineage src/orders.ts:42     # the same as autter blame --why src/orders.ts:42
```

```text
Issue ri_0123456789abcdef0123456789: TypeError: Cannot read properties of undefined (reading 'id')
  service checkout-api · environment production · status open · first seen 2026-11-14 09:12 UTC
  Repository: acme/shop

Release
  First seen in release v2.4.1
  Commit 3f9a2c1 (from GitHub release v2.4.1)
  https://github.com/acme/shop/commit/3f9a2c1d4e5f60718293a4b5c6d7e8f901234567
  Before it: release v2.4.0, commit 1a2b3c4

Pull request
  #482 Cache order lookups (from suspect commit S1)
  https://github.com/acme/shop/pull/482

Owner
  Assigned: Jane Doe, team payments, via CODEOWNERS: src/orders/ @acme/payments

Suspect commits
  S1 3f9a2c1 Cache order lookups
      by Jane Doe (@janedoe) with Claude · PR #482 · in the release window · 2026-11-13 18:00 UTC
      Why: Changed the failing line orders.ts:42.
  S2 9e8d7c6 Refactor order handler
      written by Claude, no human author · in the release window
      Why: Changed the failing function handler.

Who wrote the failing code
  src/orders.ts:42 in handler
    Claude (model claude-opus-4-5) in commit 3f9a2c1, edited by a person since
    Run by Jane Doe
    Function handler (lines 30-60): 25 of 31 lines written by AI
    Prompt: autter show-prompt s_3f9a2c1d4e5f60::t_0a1b2c3d4e5f60
  src/db.ts:10 in query
    Sam Lee in commit 1a2b3c4
    Line numbers are approximate.
  src/util.ts:5
    Not known: no authorship notes

Why this error was grouped into this issue
  Grouped by its top frames: handler@orders.ts, query@db.ts.
```

- **Release and commit**: the release the error first appeared in, the commit it runs and the release before it. When no commit is known for the release, it says so, and code was read at the default branch.
- **Suspect commits** include commits an AI agent wrote with no human author ("written by Claude, no human author"); they are never assigned an owner, since there is no person to assign.
- **Who wrote the failing code** joins up to three failing frames to the Autter authorship notes of the commit that ran: an agent and its model (and whether a person edited the line since), a person, or "Not known: no authorship notes" when the commit has none. Missing notes never count as a person. "Line numbers are approximate" means the line was matched to the deployed commit on a best-effort basis. When the agent's transcript is stored, the command prints the `autter show-prompt` command for it; it never prints prompt text itself, and `show-prompt` applies its usual access rules.
- A part Autter could not read prints one line saying so, with the reason, and the command still exits 0. No one is shown by email address.

`--repo <name>` only finds the issue in that repository (`Unknown repository: <name>` when Autter has none by that name); without it, any repository in the organization. `--org <slug>` and `--json` (the response as JSON, for scripts) work as for `autter ask`. Errors exit 1: not signed in or the wrong organization (run `autter login`, or pass `--org`), an issue that is not in this organization or repository, or the API unreachable.

`autter logs` supports severity, service, environment, message search, ISO `--from`/`--to` dates, relative `--since` durations, and repeated field filters combined with AND. Custom fields use `attributes.<exact key>`. Operators are `eq`, `neq`, `contains`, `gt`, `gte`, `lt`, `lte`, and `exists`. Default: last 24 hours, 50 events. Maximum: 90 days, 200 events. Tables abbreviate long cells; `--json` returns full fields including traces and occurrence IDs.

`autter threads` searches questions and answers, with 50 sessions per page. Use `--offset 50` for the next page. Threads are private to your account in the selected organization. New persistence cannot recover older browser-only conversations.

These commands require the updated Autter API. Authentication uses the existing CLI login flow and automatic token refresh, with no direct database access. Runtime ingest keys are for sending telemetry and cannot authenticate these commands.
