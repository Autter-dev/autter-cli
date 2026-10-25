# Capturing Sessions From Cursor, Firebender, Antigravity, Continue and OpenCode

## Summary

Autter's repository memory learns from the prompts people give coding agents (metric event 5, every transcript line, secrets redacted). Five agents were partly or wholly missing:

| Agent | Before | Now |
|---|---|---|
| Cursor, transcripts setting off | `stop` hook had no `transcript_path`, so nothing was read | The conversation is copied out of Cursor's own database on `stop` and read like a transcript |
| Firebender | Edits attributed; no transcript ever read | `stop`/`sessionEnd` and post-tool hooks hand over `transcript_path`; a reader and a sweep exist |
| Antigravity | Edits attributed; transcript kept aside | Post-tool hooks hand over `transcriptPath`; a reader and a one-minute sweep exist (lines uploaded as written; format undocumented) |
| Continue CLI (`cn`) | Only the 30-minute sweep found sessions | Swept every minute (it has no hook) |
| OpenCode on Windows | Looked in `%APPDATA%\opencode`, OpenCode writes to `~/.local/share/opencode` | XDG location first on every platform, old Windows locations still checked |

The backend change that reads OpenCode and Continue prompts (it already received them) ships separately in the monorepo.

## Changes

### Cursor without transcripts (`src/streams/cursor_state.rs`)

- Cursor keeps every conversation in `<config dir>/Cursor/User/globalStorage/state.vscdb`, table `cursorDiskKV`: `composerData:<conversationId>` lists the messages in order (`fullConversationHeadersOnly`, or inline `conversation` in older builds), and `bubbleId:<conversationId>:<bubbleId>` holds each message (`type` 1 user / 2 assistant, `text`). The hook's `conversation_id` is the composer id.
- On a `stop` hook with no `transcript_path`, `export_for_hook` opens the database read-only (2 s busy timeout), reads the conversation with one indexed range scan, and appends the messages not yet exported to `~/.autter/cursor-transcripts/<conversationId>.jsonl` as `{"role", "message": {"content": [{"type": "text", "text"}]}, "bubbleId", "createdAt", "source": "cursor-state-db"}`. The file only grows, so the Cursor reader's byte-offset watermark holds. The Cursor sweep also scans that folder.
- Any failure (no database, unknown layout, an unexpected conversation id) means no prompts for the turn, never a failed hook. `CURSOR_STATE_DB` and `AUTTER_CURSOR_EXPORT_DIR` override the paths (tests).
- The database is Cursor's private, undocumented storage; a Cursor update could change it.

### One JSONL reader for Cursor, Firebender and Antigravity (`src/streams/agents/cursor.rs`)

- `CursorAgent` takes a `JsonlFlavor` (`Cursor`, `Firebender`, `Antigravity`) that sets the tool name, the stream format (`CursorJsonl`, new `FirebenderJsonl`, new `AntigravityJsonl`, byte-offset watermarks in both format enums), discovery and the conversation id of a file.
- Firebender: `~/.firebender/projects/**/*.jsonl` (`FIREBENDER_CONFIG_DIR`), conversation id = file stem without `agent-`.
- Antigravity: `~/.gemini/antigravity*/brain/<conversationId>/.system_generated/logs/transcript.jsonl` (`ANTIGRAVITY_GEMINI_DIR`), swept every minute because the IDE may not run hooks.
- `get_agent` and the sweep registry gain `firebender` and `antigravity`.

### Presets

- Firebender: sessions key on `conversation_id` (then `completion_id`, `generation_id`, now). `stop` and `sessionEnd` map to `SessionActivity`; post-tool events carry the transcript as their stream source. The installer adds a `stop` hook, and hooks are up to date only with it.
- Antigravity: PostToolUse carries `transcriptPath` as the stream source. It is still not stored as `transcript_path`, so the commit prompt bridge never parses it.
- Cursor: `stop` falls back to the database export when `transcript_path` is null.
- OpenCode: `opencode_data_path` tries `$XDG_DATA_HOME/opencode`, `~/.local/share/opencode`, then (Windows) `%APPDATA%\opencode` and `%LOCALAPPDATA%\opencode`, taking the first that exists.

### Faster sweep for agents without hooks (`src/streams/sweep.rs`, `src/daemon/`)

- The stream worker ignored each agent's `Periodic` interval and swept everything every 30 minutes. Agents asking for less (`SweepStrategy::fast_interval`) are now also swept on a second ticker at the shortest such interval (`SweepCoordinator::run_fast_sweep`), logged at debug level. Only stale sessions are enqueued, as in the full sweep.
- Continue CLI asks for 60 s and also scans `~/.continue-cli/sessions` (newer `cn` builds).

## Still open

- Firebender's and Antigravity's transcript record formats are not documented. Lines are uploaded as written; the backend reads Cursor-style and Gemini-style user turns. A sample transcript from each would confirm or fix the prompt extraction.
- Continue CLI has no hook, so capture is up to a minute behind instead of instant.
