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
  {"schema_version":1,"error":{"code":"not_a_repository","message":"`autter stats` must be run inside a git repository (or pass -C <path>).","exit_code":2}}
  ```

  This holds for every way a command can fail, including the ones that happen
  before the command itself runs: a flag or subcommand that does not exist
  (`autter sync bogus --json`), a command that does not exist
  (`autter stat --json`), a `-C` path that cannot be entered, and a background
  service that cannot be reached. `error.exit_code` is always the process exit
  code, and `error.message` is one line.

  When the command got far enough to build its report (for example
  `stats --strict` on a commit with nothing captured, or `whoami` on a machine
  that is not signed in), the report's fields are kept next to `error`, so you
  can read both. The message is also printed to stderr, so it shows in CI
  logs; text that follows it there (usage help, a hint to run `autter doctor`)
  is not part of the document.

The JSON Schema for every document is in
[`docs/schemas/autter-cli.schema.json`](schemas/autter-cli.schema.json), one
entry under `$defs` per command (`stats`, `stats_range`, `status`, `log`,
`show`, `blame`, `diff`, `whoami`, `doctor`, `sync_status`, and `error_only`
for a command that failed before producing anything). The codes `error.code`
can take are listed in the schema's `error` entry and below.

### Error codes

| `error.code` | Exit | When |
|---|---|---|
| `usage_error` | 2 | Bad flags or arguments: an unknown option, command or subcommand, a missing or extra argument, an option value that does not parse |
| `not_a_repository` | 2 | `stats`, `status`, `log`, `show`, `blame` or `diff` run outside a git repository |
| `revision_not_found` | 1 | A commit, revision or range that does not resolve (`stats`, `log`, `show`, `diff`, and `blame --ignore-rev`) |
| `service_unreachable` | 1 | The background service could not be reached, so the command did not run (`stats`, `status`, `log`, `show`, `blame`, `diff`) |
| `note_pending` | 1 | `stats --wait` gave up before the authorship note was written |
| `not_signed_in` | 1 | `whoami` on a machine that is not signed in |
| `sync_unhealthy` | 1 | `sync status` found an upload problem |
| `check_failed` | 1 | `doctor` found a failed check |
| `runtime_error` | 1 | Anything else that went wrong (a file `blame` cannot find, a repository with no commits, a `-C` path that does not exist) |
| `nothing_captured` | 3 | `--strict`: lines were added and autter has no record of them |
| `capture_broken` | 4 | `--strict`: capture is broken (`doctor`) |
| `integrity_failed` | 5 | `verify`: an authorship note belongs to another commit or was changed after autter wrote it |

The exit code is the same with and without `--json`. `whoami`, `doctor` and
`sync status` do not read a repository and do not need the background service,
so they run outside a repository and while the service is down (`doctor`
reports the service as a failed check). `autter log` passes the options it
does not render itself to `git log`; one that git rejects is a `usage_error`,
and a revision git cannot resolve is `revision_not_found`. New codes may be
added; treat a code you do not know as a failure with the given `exit_code`.

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
`percent` has each label's share of the added lines with one decimal. The
original fields (`ai_additions`, `human_additions`, `unknown_additions`, …)
are still there for existing readers. `autter stats` also has `record`, which
says whether the commit has an Autter record and, if not, why; lines without
attribution are Untracked when it is recorded and Unknown when it is not.

The label follows the commit that added the line, in every command. In a range
(`autter stats <a>..<b>`) a line added by a commit with no authorship note is
`unknown` even when other commits in the range have one, the same as
`autter stats <that commit>` and `autter blame` report it.

`autter status` counts every uncommitted line a commit of the whole working
tree would add: edits to tracked files and new files git does not track yet
(ignored files excluded), whether or not a checkpoint saw them, so
`lines.added` equals `stats.git_diff_added_lines`. A line no checkpoint
attributes is `untracked`. With no checkpoints at all it is `untracked` while
the background service is running and `unknown` when it is not, because a
commit made then would get no record.

`capture.status` says whether autter has a record of what was added:

| `capture.status` | Meaning |
|---|---|
| `captured` | Autter has a record for at least one added line: it is `ai`, `human` or `untracked` |
| `nothing_captured` | Lines were added and every one of them is `unknown`: capture missed them. `--strict` exits 3 |
| `no_additions` | Nothing was added |

A commit typed entirely by hand while capture was working has a record and
only `untracked` lines: it is `captured`. To require AI or known-human lines,
check `lines.ai` and `lines.human` yourself.

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
| 3 | Nothing captured: autter has no record of the added lines (`stats`, `status`; only with `--strict`) |
| 4 | Capture is broken (`doctor`; only with `--strict`) |
| 5 | A note failed the integrity check (`verify`) |

`autter --help` prints the same table.

## `--strict`

Without `--strict`, nothing changes: `stats` and `status` exit 0 whatever was
captured, and `doctor` exits 1 when any check fails. With `--strict` (or
`AUTTER_STRICT=1` in the environment):

- `autter stats [<commit>|<a>..<b>] --strict` exits **3** when lines were added
  and autter has no record of any of them: capture missed the commit (the
  background service was not running, the commit was made before autter was set
  up, or it was made on a machine without autter). "Has a record" means the
  commit has an authorship note, which is what `record.status` = `recorded` and
  `capture.note_present` report; every added line is then `ai`, `human` or
  `untracked`, never `unknown`.
  - A commit with AI lines exits 0.
  - A commit typed entirely by hand, with no agent and no editor extension,
    exits 0 when it has a record: capture worked and found no agent. Its lines
    are `untracked` and `capture.status` is `captured`.
  - The same commit with no record exits 3: its lines are `unknown`.
  - A commit that adds no lines passes.
  - A merge commit is judged on the lines it adds itself: the lines of the
    merge that are in none of its parents (a conflict resolution, an edit made
    while merging). What the merged branches added belongs to their own
    commits. A merge with no lines of its own has nothing to capture and
    passes; one with such lines follows the rules above.
  - A range exits 3 when every added line comes from a commit with no record.
    One recorded commit that still contributes lines is enough to pass; use
    `lines.unknown` to require that none is missing.
- `autter status --strict` exits **3** when there are uncommitted changes, no
  checkpoint was recorded for them and the background service is not running,
  so a commit made now would get no record. Changes typed by hand while the
  service is running exit 0, and so do changes a checkpoint covers.
- `autter doctor --strict` exits **4** when a check that capture depends on
  fails (background service, git trace2, the end-to-end checkpoint, agent
  hooks), and 1 when only other checks fail. The report has `capture_ok`.

## `autter verify`: the integrity check

Authorship notes live in `refs/notes/ai`, which anyone who can push to the
repository can change. Every note autter writes, including the notes it
rewrites after a rebase, amend, cherry-pick, reset, stash or `autter ci`
merge, records the commit it was written for, that commit's tree, the blob of
each file it attests, and a SHA-256 digest of the note. `autter verify`
checks them:

```bash
autter verify                      # HEAD
autter verify <sha>                # one commit
autter verify origin/main..HEAD    # every commit in a range
autter verify origin/main..HEAD --json --strict
```

| Status | Exit | Meaning |
|---|---|---|
| `verified` | 0 | Written for this commit and unchanged since |
| `legacy` | 0 (warning) | Written before the integrity check (autter 3.7.1 and earlier). Its commit and line ranges are still checked |
| `missing` | 0 (warning) | The commit has no note |
| `pending_rewrite` | 0 (warning) | Git copied another commit's note here during a rebase or amend and autter has not rewritten it yet (the background service was not running). The two commits carry the same patch |
| `rebound_by_older_autter` | 0 (warning); 5 with `--strict` | An autter release older than the integrity check moved the note here and kept the old stamp, so the digest cannot be checked. The stamp's tree and blobs are checked against the commit it names (`recorded_commit`) when that commit is in this clone |
| `unavailable` | 0 (warning) | The commit or its files are not in this clone (a shallow or partial clone), so the note could not be checked. This is not tampering |
| `wrong_commit` | 5 | The note was written for another commit with a different change: it was copied or moved |
| `content_mismatch` | 5 | The note's record of the commit's files does not match the commit; for a note that looks rebound, its stamp does not describe the commit it names (`recorded_commit`) |
| `range_out_of_bounds` | 5 | The note attributes lines past the end of a file, or lines of a file the commit does not have |
| `digest_mismatch` | 5 | The note was changed after autter wrote it |
| `carried_failure` | 5 | Autter derived the note, during a rebase, amend, cherry-pick or `autter ci` merge, from a note that failed the check, or an older autter rebound it from a note that fails the check. `recorded_commit` names that source commit |

`--json` prints `{schema_version, range, commits, counts, failed, results}`,
each result `{commit, status, detail, recorded_commit?}`; on exit 5 the same
report comes with `error.code` = `integrity_failed`, and `failed` counts the
commits that made it fail.

`--strict` fails `rebound_by_older_autter` (exit 5; the status stays
`rebound_by_older_autter` and the human report marks it `FAIL`): its digest
cannot be checked, so anyone who can write notes could otherwise turn a
failure into a warning by making a note look rebound. To
clear it, rewrite the commits with this autter (a rebase or `autter ci`
merge), which stamps them again. `--strict` still passes `legacy` and
`missing` notes, with a warning on stderr. **A later release will fail them
under `--strict`**, once notes written before the integrity check have aged
out of the ranges you check.

`autter stats`, `autter blame` and the range stats do not trust a note that
fails: its lines count as `unknown`, and they print one line naming the commit
and pointing at `autter verify`.

A rewrite does not make a failing note valid. Before a rebase (including
reorder, squash and fixup), amend, cherry-pick or `autter ci` merge carries
attribution to new commits, autter checks the notes it carries from, all in
one pass. A new commit built from a failing note gets a note that fails with
`carried_failure`, naming the source commit, so the evidence stays visible;
a squash built from several notes fails if any of them does. Rewrites that
put attribution back into the working tree (`reset`, `cherry-pick -n`,
`merge --squash`) read it through blame, which skips a failing note: those
lines are committed again as unknown. A source note that is `unavailable`
(objects missing from a shallow clone) is carried as before.

The same holds for the other ways attribution comes back:

- **`git stash`.** The attribution saved with a stash (`refs/notes/ai-stash`)
  is stamped for the stash commit. `stash pop`/`apply` checks it first; a note
  written for another stash, or changed after autter wrote it, is not restored
  (those lines count as untracked, as any edit autter has no record of) and
  autter prints one warning line naming the stash (in the background service's
  log when the service handles the stash). A stash note without a stamp,
  written by autter 3.8.1 or earlier, is restored as before, so stashes made
  before an upgrade keep their AI lines; a stamped note with its stamp removed
  looks the same and is restored too.

This is an integrity check, not tamper evidence. It catches notes copied to
another commit, moved, or edited by hand. Not caught:

- someone who can write to the repository and recomputes every field of the
  note (the digest is not a signature), including a stash note; a record kept
  outside the repository is planned for that;
- a note made to look rebound with a genuine stamp copied from another note
  whose commit is not in the clone, or whose claims hold: it is a warning
  without `--strict`, and a rewrite by this autter carries it like any other
  warning;
- edits to the working log in `.git/ai/`, which the next commit records as
  they are.

To fail a pull request whose notes do not belong to its commits, run it next
to `autter ci` (or on its own after `autter fetch-notes origin`):

```yaml
      - name: Check authorship notes
        run: autter verify origin/${{ github.base_ref }}..HEAD --strict
```

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
            echo "::error::autter has no record of anything in this pull request. Run 'autter doctor' on the machine that wrote it."
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
percentage check. A pull request written by hand on a machine where autter is
working passes: exit 3 means autter has no record, not that no AI was used. To check a single commit, use `autter stats <sha> --json
--strict` instead of a range.
