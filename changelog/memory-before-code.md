# Repository Memory Before Code

## Summary

Autter keeps a shared memory per repository (written nightly from the team's agent sessions, reviews, production, CI, scans, docs, the code index and the wiki). Until now an agent saw it only if it called an MCP tool. With this change, `autter install` adds an `autter memory-hook` next to the checkpoint hooks of every agent whose hooks can add text to the model's context, so the memory is in the session before the agent writes code:

- **Session start:** the repository memory map (conventions, gotchas, workflows, docs vs practice, open threads), what has not worked, and the list of area pages. Once per session.
- **Each prompt:** the memory sections that match the request (by meaning, then by words).
- **A file read or edited:** the area page covering it, once per area per session. The session briefing carries the area map, so a file outside a new area makes no call.

The hook calls `GET /api/cli/orgs/<org>/repo-memory/brief` with the CLI token, gives up after 3 seconds, and prints nothing on any failure (not signed in, offline, a repository Autter does not know). Output stays under 9,500 characters (Claude Code caps hook context at 10,000; Codex at about 2,500 tokens by default).

## Coverage

Researched 2026-10-25 from each agent's hook documentation.

| Agent | Events installed | Output | Notes |
|---|---|---|---|
| Claude Code | `SessionStart`, `UserPromptSubmit`, `PostToolUse` (`Read\|Edit\|Write\|MultiEdit\|NotebookEdit`) | `hookSpecificOutput.additionalContext` | Context from PostToolUse arrives with the tool result. [hooks](https://code.claude.com/docs/en/hooks.md) |
| Codex | `SessionStart`, `UserPromptSubmit`, `PostToolUse` | `hookSpecificOutput.additionalContext` | Trusted in `config.toml` like the checkpoint hooks. [hooks](https://learn.chatgpt.com/docs/hooks) |
| Gemini CLI | `SessionStart`, `BeforeAgent`, `AfterTool` (`read_file\|read_many_files\|write_file\|replace`) | `hookSpecificOutput.additionalContext` (no event name) | [hooks reference](https://geminicli.com/docs/hooks/reference/) |
| Droid | `SessionStart`, `UserPromptSubmit`, `PostToolUse` (`Read\|Edit\|Write\|Create\|ApplyPatch\|MultiEdit`) | plain stdout (session, prompt); `hookSpecificOutput` (PostToolUse) | Droid ignores `settings.json` hooks when `~/.factory/hooks.json` exists. [hooks](https://docs.factory.com/reference/hooks-reference) |
| GitHub Copilot (CLI and VS Code) | `SessionStart`, `PostToolUse` (with `--event`) | top-level `additionalContext` and `hookSpecificOutput` | Copilot drops prompt-hook output. [CLI hooks](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-hooks-reference) |
| Cursor | `sessionStart` | `additional_context` | Cursor documents it but currently drops it ([forum](https://forum.cursor.com/t/158452)); postToolUse is not installed until that is fixed, so no call is added to every tool call. |
| OpenCode | plugin: `experimental.chat.system.transform`, `chat.message` | system prompt | The briefing and the sections for the latest prompt, cached per session. |
| Amp | plugin: `agent.start` | hidden message | |
| Pi | extension: `before_agent_start` | hidden `autter-memory` message | |
| Windsurf, Continue CLI | none | | Windsurf hooks cannot add context; Continue's hooks are not wired yet (it reads `~/.claude/settings.json`, so Claude's hooks will apply once it is). Use the `autter-repo-context` skill or `autter memory search`. |

## Also

- In a terminal, `autter memory` (see `changelog/repository-memory-search.md`) prints the same memory: `show` for the map, `for <file>…` for the area pages, `search "<question>"` for the matching sections. The hook does not need it.
- The `autter-repo-context` skill in [Autter-dev/autter-skills](https://github.com/Autter-dev/autter-skills) (`npx skills add Autter-dev/autter-skills --skill autter-repo-context`) tells agents to load the memory for the files they are about to change, through the MCP tools or `autter memory`, and to record durable lessons back. It lives there rather than in the CLI so agents without the CLI's hooks (Cursor, Windsurf, Continue) get it too, and so there is one skill for reading Autter before coding.

## Privacy and switches

- `autter config set feature_flags.memory_context false` (or `AUTTER_MEMORY_CONTEXT=false`) turns the hook off.
- In `local` prompt storage mode the prompt text is never sent; the session and file briefings send only the repository and file paths.
- Repository allow/exclude filters apply.
- Per-session state lives in `~/.autter/memory-sessions/` (what was already injected) and is pruned after three days.
