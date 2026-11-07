# How edits made through an agent's shell tool are captured

Agents edit files through their file-edit tools (Claude Code's `Edit` and `Write`) and through their shell tool (`echo >> file`, `sed -i`, a formatter, a code generator). A shell tool call names no file paths, so autter finds what it changed by comparing file metadata before and after the call.

## How it works

1. **Before the call (PreToolUse).** autter scans the working tree and records size, mtime and inode for every non-ignored file that changed since the last checkpoint, plus every file modified in the last couple of seconds. The snapshot goes to the background service and is also written to `.git/ai/bash_snapshots/`. Files that already had uncheckpointed changes get an untracked checkpoint, so someone else's edit is not credited to the agent.
2. **After the call (PostToolUse).** autter loads the snapshot from the background service, or from disk if the service was not running or restarted. It scans again and gives the agent every file that was created or whose metadata changed. Only the lines that changed are attributed: the AI checkpoint diffs each file against its last checkpointed content. The snapshot file is then deleted.

## Requirements

- The agent's hooks must fire on its shell tool. For Claude Code, `autter install-hooks` uses the catch-all matcher `"*"`. Installs from older autter versions matched only `Write|Edit|MultiEdit`, which skips the `Bash` tool, so shell edits were committed as untracked. `autter doctor` and `autter debug` now report this as "Claude Code shell edits: NOT captured"; run `autter install-hooks` to fix it.
- The background service is optional for this: if it isn't running, the snapshot on disk is used.

## What is not captured

- Gitignored files, and files matched by `.autter-ignore` or the default ignore patterns (lock files, `node_modules`, ...).
- Calls where the scan takes longer than 1.5s, or where more than 50,000 files changed recently. The whole call is skipped and its edits are committed as untracked.
- Deletions. Deleted lines need no attribution.

## Known limitation

A file edited by something else *while* the shell command runs (for example, you save it in your editor during a long build) cannot be told apart from the agent's change, and is attributed to the agent.
