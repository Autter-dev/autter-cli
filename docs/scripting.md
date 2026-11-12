# Scripting autter and using it in CI

Every reporting command can print machine-readable output and exits with a
code a pipeline can act on. This page is the contract.

## `--json`

These commands accept `--json`: `stats`, `status`, `log`, `show`, `blame`,
`diff`, `whoami`, `doctor` and `sync status`. The flag can go before or after
the subcommand (`autter --json stats` and `autter stats --json` are the same).

- **Stdout carries exactly one JSON document and nothing else.** Warnings,
  progress and diagnostics go to stderr. (`blame --json` is pretty-printed over
  several lines; every other command prints one line.)
- **Every document has `schema_version`** (currently `1`). Fields are only ever
  added. A change that would break a reader (a removed or retyped field) bumps
  the version.
- **A failure still prints one document**, with an `error` object, and exits
  non-zero:

  ```json
  {"schema_version":1,"error":{"code":"not_a_repository","message":"Failed to find repository: …","exit_code":2}}
  ```

  When the command got far enough to build its report (for example
  `stats --strict` on a commit with nothing captured), the report's fields are
  kept next to `error`, so you can read both. The message is also printed to
  stderr, so it shows in CI logs.

The JSON Schema for every document is in
[`docs/schemas/autter-cli.schema.json`](schemas/autter-cli.schema.json), one
entry under `$defs` per command (`stats`, `stats_range`, `status`, `log`,
`show`, `blame`, `diff`, `whoami`, `doctor`, `sync_status`, and `error_only`
for a command that failed before producing anything).

### Error codes

| `error.code` | Exit | When |
|---|---|---|
| `usage_error` | 2 | Bad flags or arguments |
| `not_a_repository` | 2 | Run outside a git repository |
| `revision_not_found` | 1 | A commit or range that does not resolve |
| `note_pending` | 1 | `stats --wait` gave up before the authorship note was written |
| `not_signed_in` | 1 | `whoami` on a machine that is not signed in |
| `sync_unhealthy` | 1 | `sync status` found an upload problem |
| `check_failed` | 1 | `doctor` found a failed check |
| `runtime_error` | 1 | Anything else that went wrong |
| `nothing_captured` | 3 | `--strict`: nothing was captured |
| `capture_broken` | 4 | `--strict`: capture is broken (`doctor`) |

### Line labels

`stats`, `status` and `log` report added lines under the same four labels the
terminal output uses:

| Field | Meaning |
|---|---|
| `lines.ai` | Written by an AI agent |
| `lines.human` | Typed by a known human (editor extension) |
| `lines.untracked` | The commit has an authorship note, but it does not attest these lines |
| `lines.unknown` | The commit has no authorship note at all, so autter has no data |

`lines.ai + lines.human + lines.untracked + lines.unknown = lines.added`.
`percent` has each label's share of the added lines with one decimal, and
`capture.status` is `captured` (at least one AI or known-human line),
`nothing_captured` (lines were added and none is attributed) or `no_additions`.
The original fields (`ai_additions`, `human_additions`, `unknown_additions`, …)
are still there for existing readers. `autter stats` also has `record`, which
says whether the commit has an Autter record and, if not, why; lines without
attribution are Untracked when it is recorded and Unknown when it is not.

```json
{
  "schema_version": 1,
  "commit": "3f2a…",
  "human_additions": 1, "unknown_additions": 0, "ai_additions": 2, "ai_accepted": 2,
  "git_diff_deleted_lines": 0, "git_diff_added_lines": 3,
  "tool_model_breakdown": { "claude::claude-sonnet-4-5": { "ai_additions": 2, "ai_accepted": 2 } },
  "lines": { "added": 3, "deleted": 0, "ai": 2, "human": 1, "untracked": 0, "unknown": 0 },
  "percent": { "ai": 66.7, "human": 33.3, "untracked": 0.0, "unknown": 0.0 },
  "capture": { "status": "captured", "note_present": true },
  "record": { "status": "recorded" }
}
```

### `blame --json`

`lines` maps the AI lines to the prompt that wrote them, as `"<line>"` or
`"<start>-<end>"` keys. `line_categories` covers every blamed line with the
same keys and the label names above (`ai`, `human`, `untracked`, `unknown`),
and `category_counts` has the number of lines under each label. In `prompts`,
`accepted_lines` and the other line counts are the ones in the commit's
authorship note.

```json
{
  "schema_version": 1,
  "lines": { "3-4": "s_405e2e0215c7c0::t_856b3232f4045d" },
  "line_categories": { "1": "untracked", "2": "human", "3-4": "ai", "5": "untracked", "6-7": "unknown" },
  "category_counts": { "ai": 2, "human": 1, "untracked": 2, "unknown": 2 },
  "prompts": { "s_405e2e0215c7c0::t_856b3232f4045d": { "accepted_lines": 2, "…": "…" } },
  "metadata": { "is_logged_in": false, "current_user": "Ada <ada@example.com>" }
}
```

```json
{
  "schema_version": 1,
  "commit": "3f2a…",
  "human_additions": 1, "unknown_additions": 0, "ai_additions": 2, "ai_accepted": 2,
  "git_diff_deleted_lines": 0, "git_diff_added_lines": 3,
  "tool_model_breakdown": { "claude::claude-sonnet-4-5": { "ai_additions": 2, "ai_accepted": 2 } },
  "lines": { "added": 3, "deleted": 0, "ai": 2, "human": 1, "untracked": 0, "unknown": 0 },
  "percent": { "ai": 66.7, "human": 33.3, "untracked": 0.0, "unknown": 0.0 },
  "capture": { "status": "captured", "note_present": true },
  "record": { "status": "recorded" }
}
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Runtime error |
| 2 | Usage error (bad flags, outside a repository) |
| 3 | Nothing captured (`stats`, `status`; only with `--strict`) |
| 4 | Capture is broken (`doctor`; only with `--strict`) |

`autter --help` prints the same table.

## `--strict`

Without `--strict`, nothing changes: `stats` and `status` exit 0 whatever was
captured, and `doctor` exits 1 when any check fails. With `--strict` (or
`AUTTER_STRICT=1` in the environment):

- `autter stats [<commit>|<a>..<b>] --strict` exits **3** when lines were added
  and none of them is attributed to AI or a known human. A commit that adds no
  lines passes.
- `autter status --strict` exits **3** when there are uncommitted changes and
  none of them is attributed.
- `autter doctor --strict` exits **4** when a check that capture depends on
  fails (background service, git trace2, the end-to-end checkpoint, agent
  hooks), and 1 when only other checks fail. The report has `capture_ok`.

## CI behaviour

When `CI` is set (`CI=true` on GitHub Actions, GitLab, CircleCI, Buildkite…)
or stdout is not a terminal, autter prints no color, never prompts, shows no
spinners or full-screen views, and never offers an update. `--json` also
turns color off. Set `CI=false` to opt out.

## GitHub Actions example

Fail a pull request when nothing in it was captured, or when the AI share of
its added lines is above a limit:

```yaml
name: AI attribution
on: pull_request

jobs:
  attribution:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Install autter
        run: |
          curl -fsSL https://api.autter.dev/install.sh | bash
          echo "$HOME/.autter/bin" >> "$GITHUB_PATH"

      - name: Fetch authorship notes
        run: autter fetch-notes origin

      - name: Check AI share
        env:
          RANGE: ${{ github.event.pull_request.base.sha }}..${{ github.event.pull_request.head.sha }}
          MAX_AI_PERCENT: "80"
        run: |
          set +e
          autter stats "$RANGE" --json --strict > stats.json
          code=$?
          set -e

          if [ "$code" -eq 3 ]; then
            echo "::error::Nothing in this pull request was captured by autter. Run 'autter doctor' on the machine that wrote it."
            exit 1
          elif [ "$code" -ne 0 ]; then
            echo "::error::autter stats failed: $(jq -r '.error.message' stats.json)"
            exit "$code"
          fi

          ai=$(jq '.percent.ai' stats.json)
          echo "AI share of added lines: ${ai}%"
          if jq -e --argjson max "$MAX_AI_PERCENT" '.percent.ai > $max' stats.json > /dev/null; then
            echo "::error::AI share ${ai}% is above ${MAX_AI_PERCENT}%"
            exit 1
          fi
```

To only fail when nothing was captured, keep the `--strict` call and drop the
percentage check. To check a single commit, use `autter stats <sha> --json
--strict` instead of a range.
