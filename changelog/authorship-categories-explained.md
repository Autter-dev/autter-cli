# Authorship categories, explained the same way everywhere

## Summary

`stats`, `blame`, `show`, `log` and `status` now use one set of labels, say why lines have no attribution instead of printing the same five repair commands, and wait for the background service when a commit's record is still being written.

## Labels

Every added line is one of:

- **AI**: an agent Autter captures wrote it.
- **human**: typed in an editor with the Autter extension.
- **untracked**: Autter recorded the commit, but no agent or editor extension recorded the line. Usually typed by hand.
- **unknown**: the commit has no Autter record at all.

`autter stats` prints each category's line count and share under the bar (`3 human (30%) · 2 untracked (20%) · 5 AI (50%) · 10 added, 1 deleted`) and says in one line what the AI share measures: lines an agent wrote into the file, not who directed or reviewed the work.

`autter blame` on a terminal starts each line with its category and ends with a legend for the categories shown. The author column is still the git author. Piped output is unchanged and stays git-compatible; `--markers` and `--no-markers` override the default.

## Why a commit has no attribution

When lines have no attribution, `stats`, `show` and `log` now say which of these applies, with at most one next step:

- **No AI activity detected**: Autter recorded the commit and no agent wrote its lines. Nothing to fix.
- **Pending**: the background service is still writing the record. `stats`, `show` and `log` wait up to 10 seconds for it before saying so.
- **Predates Autter**: the commit is older than the first commit Autter recorded in the repository.
- **Capture failed**: checkpoints were recorded, but no record was written. Next: `autter doctor`.
- **Made by someone else**: their record may not be fetched. Next: `autter fetch-notes`.
- **Made while Autter was not recording**. Next: `autter doctor`.

On a terminal, `autter stats` also names the agents captured on this machine, or links to the supported tools when none is set up. `autter log` no longer prints a troubleshooting block under every commit typed by hand.

`autter status` waits up to 3 seconds for checkpoints still queued in the background service and shows "N checkpoints are still being written" when they haven't landed. With nothing to show it says "No AI activity recorded since the last commit" instead of suggesting `install-hooks`.

## Coverage

`autter stats --coverage [<rev>|<range>]` lists how many of the last 50 commits (or the range) have a record, how many predate Autter, are pending or are missing a record and why, which merges have none, and whether the records are on the remote as of the last fetch. `--json` gives the same report.

## Fixes

- `autter stats A..B` printed only blank lines when piped. It now prints the same summary as on a terminal, and counts known-human lines in a range as human instead of untracked.
- A commit note's per-session `accepted_lines` counted AI lines that were never committed (unstaged, in other files, or deleted by hand after the last checkpoint). It now counts the lines in the commit, matching `stats --json` `ai_accepted`.
- `stats`, `blame`, `diff`, `show`, `status` and `log` outside a repository print "`autter <command>` must be run inside a git repository" instead of git's `rev-parse` failure.

## JSON

`stats --json` keeps its fields and adds `record`: `{"status": "recorded"}`, `{"status": "pending", "pending": N}`, `{"status": "predates_autter", "first_recorded_at": <unix seconds or null>}` or `{"status": "missing", "cause": "capture_failed" | "other_author" | "not_captured"}`. `unknown_additions` still counts both untracked and unknown lines; `record` tells them apart. `status --json` adds `pending_checkpoints`.
