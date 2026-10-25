# Session-Activity Hooks for Every Coding Agent

## Summary

The daemon uploads every line of an agent's transcript (metric event 5, secrets redacted), but it only learned that a transcript had changed when a tool call **edited a file**. Hooks for read-only tools carry no file paths, so they produced no checkpoint request, and a checkpoint request is what tells the stream worker to read the transcript. Sessions that asked questions, reviewed code or planned without editing waited for the periodic sweep (up to 30 minutes), and the last turns of every session did too.

Autter's repository memory reads these session events to learn what people ask agents and the corrections they give them, including in sessions that never commit. With this change, every agent whose hook system or plugin API has a prompt-submitted or turn-ended event wakes the transcript reader at that moment, so those sessions arrive within seconds.

## Coverage

| Agent | Events | How |
|---|---|---|
| Claude Code | `UserPromptSubmit`, `Stop`, `SubagentStop` | hooks in `settings.json` (no matcher) |
| Codex | `UserPromptSubmit`, `Stop` | inline `[hooks]` in `config.toml` (or `hooks.json`), trusted like the tool hooks; `Stop` was already installed but ignored |
| Droid | `UserPromptSubmit`, `Stop`, `SubagentStop` | hooks in `~/.factory/settings.json` (no matcher) |
| Gemini CLI | `BeforeAgent`, `AfterAgent` | hooks in `~/.gemini/settings.json` (no matcher) |
| Cursor | `stop` | `~/.cursor/hooks.json`; needs Cursor transcripts on (`transcript_path` is null otherwise) |
| GitHub Copilot (VS Code and CLI) | `UserPromptSubmit`, `Stop` | `~/.copilot/hooks/autter.json`, read by both; the CLI takes these as aliases of `userPromptSubmitted` / `agentStop` |
| Windsurf | `post_cascade_response_with_transcript` (also `post_cascade_response`, `pre_user_prompt`) | already installed but ignored; now mapped |
| OpenCode | `session.idle` | plugin `event` hook |
| Amp | `agent.end` | plugin; now also sends the thread id |
| Pi | `agent_end` | extension |

Not covered, because there is no transcript the daemon reads for them or no hook point: Firebender, Antigravity, Continue CLI, and the JetBrains, VS Code-extension and Visual Studio integrations. Their sessions are still read by the periodic sweep where the agent has a stream reader. (Firebender, Antigravity, Continue CLI and Cursor without transcripts are covered by `agent-capture-gaps.md`.)

## Changes

### Presets (`src/commands/checkpoint_agent/presets/`)

- New `ParsedHookEvent::SessionActivity` and `session_activity_event(context, stream_source)`: the transcript changed without a file edit. Without a transcript to read, the hook is skipped (preset error, exit 0).
- Each preset above maps its events to it before any tool classification. On events that run before the agent acts on a prompt (`UserPromptSubmit`, `BeforeAgent`), Claude and Gemini skip the model lookup, which reads the transcript, so the hook adds no noticeable delay. The stream worker extracts the model itself.
- Copilot: one hooks file serves VS Code and the CLI. A transcript under VS Code's chat storage is VS Code's; otherwise the CLI's `~/.copilot/session-state/<session>/events.jsonl`. A Claude transcript (VS Code also runs Claude Code hooks) is left to the Claude preset.
- Pi: `tool_name` and `model` are optional in the hook input; `agent_end` concerns no tool.

### Orchestrator, daemon, checkpoint command

- `execute_session_activity` builds one `CheckpointRequest` with **no files** and the stream source, marked `session_activity=true`, with the repository (from the hook's `cwd`) as `repo_work_dir`. The daemon takes the repository from there and records no checkpoint; daemons older than this CLI ingest a file-less request as a no-op after the same wake-up.
- Repository allow/exclude filters apply through that repository; with filters configured, activity outside any repository is skipped.
- `session_activity_requests=N` is printed on its own line in test mode, so `checkpoint_requests=N` still counts only requests that record a checkpoint.

### Installers (`src/mdm/`)

- `ensure_session_hook` / `session_hooks_installed` (shared by Claude Code, Droid, Gemini CLI): one autter command per event in a block without a matcher, updated in place when the binary moved, duplicates removed, user hooks on the same events left alone.
- Cursor, Copilot and Codex add their events next to the tool hooks.
- Every installer counts an install as up to date only when the new events are present, so `autter install` and upgrades add them. Uninstall removes them too.
- The OpenCode, Amp and Pi plugin files are rewritten on install as before (content comparison), which ships the new handlers.

## Behaviour Guarantees

| Scenario | Before | After |
|---|---|---|
| Session that never edits a file | transcript read on the next sweep (≤ 30 min) | read on each prompt and turn end |
| Last turns after the final edit | next sweep | end of the turn |
| Prompt hooks' output | n/a | nothing on stdout (Claude Code adds `UserPromptSubmit` output to the prompt) |
| Repository excluded by filters | n/a | no request sent |
| New CLI with an older daemon | n/a | request ingested as a no-op after waking the stream worker |
| Re-running `autter install` | n/a | idempotent; user hooks on the same events kept |
| Cursor with transcripts off | n/a | `stop` hook is a no-op |
