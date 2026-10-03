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
```

Questions use captured Runtime evidence and source code to explain root causes, impact, and recommended fixes. Hypotheses and telemetry gaps should be explicit. Threads are saved automatically; you can resume them from Dashboard or CLI. Historical conclusions need fresh evidence before reuse.

Use `--org <slug>` for another organization you belong to and `--json` for complete structured output. `--mode codebase` and `--mode reviews` select the existing assistants. Resuming with `--thread` restores that thread's mode and repository unless you explicitly specify them; changing its scope requires a new thread.

`autter logs` supports severity, service, environment, message search, ISO `--from`/`--to` dates, relative `--since` durations, and repeated field filters combined with AND. Custom fields use `attributes.<exact key>`. Operators are `eq`, `neq`, `contains`, `gt`, `gte`, `lt`, `lte`, and `exists`. Default: last 24 hours, 50 events. Maximum: 90 days, 200 events. Tables abbreviate long cells; `--json` returns full fields including traces and occurrence IDs.

`autter threads` searches questions and answers, with 50 sessions per page. Use `--offset 50` for the next page. Threads are private to your account in the selected organization. New persistence cannot recover older browser-only conversations.

These commands require the updated Autter API. Authentication uses the existing CLI login flow and automatic token refresh, with no direct database access. Runtime ingest keys are for sending telemetry and cannot authenticate these commands.
