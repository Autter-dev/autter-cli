# VS Code Copilot Agent Edits Checkpoint Without a Transcript, and Doctor Says Whether Capture Works

## Summary

VS Code's built-in Copilot agent only puts `transcript_path` in its hook payload once it has written a session transcript, and it writes one only when the window has workspace storage. With no folder open it also sends no `cwd`. Autter used a missing `transcript_path` to mean "this is the Copilot CLI", so those VS Code hooks went to the CLI parser, which does not know VS Code's tool names (`replace_string_in_file`, `create_file`, `multi_replace_string_in_file`, `insert_edit_into_file`, `run_in_terminal`) and dropped every edit as a "non-edit tool". The AI's lines then came out untracked. Nothing in this path depends on local or connected mode; it hit both.

`autter doctor` checked only that the hook file and settings existed, so it reported the chain as complete while edits were being dropped.

## Changes

### Routing (`src/commands/checkpoint_agent/presets/github_copilot/`)

- A PreToolUse/PostToolUse payload without `transcript_path` goes to the VS Code parser when its tool name is VS Code-only, or its tool-call id has the `__vscode-<n>` suffix VS Code adds to every tool call (`ide::is_vscode_copilot_payload`). Everything else still goes to the Copilot CLI parser; `apply_patch`, used by both, is told apart by the id.
- The VS Code parser accepts a missing `transcript_path` for those payloads (no stream source, model `unknown`), and when `cwd` is missing it uses the directory of the edited file.
- A session event (`UserPromptSubmit`/`Stop`) without `cwd` is reported as a skip, not an error.

### Hook heartbeat (`src/commands/checkpoint_agent/hook_heartbeat.rs`)

- `autter checkpoint github-copilot` records each run in `~/.autter/internal/hook_heartbeats/github-copilot.json`: when it last ran, the outcome (checkpoint, prompt/turn event, skipped non-edit tool, dropped edit), the time of the last checkpoint, and the time and reason of the last dropped edit. Best-effort; it never fails the hook.

### Doctor (`src/commands/debug.rs`, `src/commands/doctor.rs`)

- The hook file must give both PreToolUse and PostToolUse an `autter checkpoint github-copilot` command whose binary exists; an unparsable file, a missing event or a stale binary path makes the chain incomplete.
- An unset `chat.hookFilesLocations` counts as enabled on VS Code ≥ 1.144, whose built-in default includes `~/.copilot/hooks` (a user-set value replaces the defaults, so it must list the dir). `chat.useHooks: false` makes the chain incomplete.
- With the chain in place, the check reports from the heartbeat: working (last Copilot checkpoint N ago), hooks running with no edit yet, no activity yet, or a warning with the reason when the latest Copilot edit was dropped. An incomplete chain with a checkpoint in the last week is a warning, not a failure (hooks can also come from a workspace `.github/hooks` file).

## Still open

- Tests were written but not run for this change.
- The heartbeat is recorded for the `github-copilot` preset only.
