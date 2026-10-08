# AI agents autter cannot fully capture

autter attributes a line to an AI agent only when the agent tells autter, through a
hook or plugin, right before and right after it edits a file. When an agent has no
such integration, its edits look like any other change: `autter stats` counts them
as **untracked**, and `autter blame` credits whoever committed them.

`autter doctor` warns when it finds one of the tools below, and `autter stats`
prints a note when most of a commit is untracked and one of them is installed.

## Google Antigravity

**Status: supported through hooks; the Antigravity IDE may not run them.**

`autter install-hooks` adds an `autter` entry to Antigravity's global hooks file,
`~/.gemini/config/hooks.json`. It registers a `PreToolUse` and a `PostToolUse` hook
for the file-editing tools (`write_to_file`, `replace_file_content`,
`multi_replace_file_content`) and `run_command`. Each hook runs
`autter checkpoint antigravity-pre|antigravity-post --hook-input stdin` and replies
`{}`, so it never changes Antigravity's approval decisions. Other entries in the
file are left alone, and `autter uninstall-hooks` removes only the `autter` entry.

Antigravity is detected by any of: the `agy` launcher on `PATH`, `~/.local/bin/agy`
(macOS/Linux), `%LOCALAPPDATA%\agy\bin\agy.exe` (Windows), or one of the app data
directories `~/.gemini/antigravity` (Antigravity 2.0), `~/.gemini/antigravity-cli`
(CLI) and `~/.gemini/antigravity-ide` (IDE). A plain `~/.gemini` directory is not
a signal, because the Gemini CLI uses it too. The desktop app bundle and install
folder names are not documented, so they are not used.

What works and what doesn't:

| Surface | Hooks run? | Evidence |
| --- | --- | --- |
| Antigravity CLI (`agy`) | Yes | Community testing (CLI 1.1.10: hooks fired on every tool call); the CLI changelog's hook fixes |
| Antigravity 2.0 desktop app | Expected | The 2.0 changelog: "Custom hooks you define now run at the end of a turn instead of being skipped" and other hook fixes. A community report against 2.5.0 saw no hook calls. |
| Antigravity IDE | Not confirmed | The docs list IDE hook paths, but community reports against IDE 1.107.0 and 2.1.1 saw zero hook invocations, and the changelog has no IDE hook entries |

What is still missing:

- **IDE hook execution.** Until the Antigravity IDE runs `hooks.json` hooks, its
  agent edits stay untracked. Nothing on autter's side can fix this; it needs an
  Antigravity IDE build that executes `PreToolUse`/`PostToolUse` hooks. To check
  your build, edit a file with the IDE agent, commit, and run `autter stats`: AI
  lines mean the hooks ran.
- **Transcripts.** Hooks pass `transcriptPath`, a `transcript.jsonl` under
  `<app data dir>/brain/<conversationId>/.system_generated/logs/`, but its record
  format is not documented, so autter does not import Antigravity prompts yet.
  A documented (or stable, versioned) transcript schema would unblock this.
- **Approval replies.** A `PreToolUse` hook must print a JSON reply. autter prints
  `{}` (no `decision`), which the CLI changelog says is handled safely ("safely
  handling empty decision strings returned by pre-tool hooks"). If a future build
  starts rejecting replies without a `decision`, the hook would need to send an
  explicit pass-through value, and none of the documented values (`allow`,
  `deny`, `ask`, `force_ask`, `deny_unless_prior_grant`) currently means
  "no opinion".

Sources (checked 2026-10-08):

- Hooks reference: https://antigravity.google/docs/hooks
- Settings (IDE app data dir): https://antigravity.google/docs/settings
- CLI install paths: https://antigravity.google/docs/cli/install and
  https://antigravity.google/docs/cli/troubleshooting
- Changelog: https://antigravity.google/docs/changelog
- IDE/2.0 hooks not executing:
  https://discuss.ai.google.dev/t/do-antigravity-ide-2-0-actually-execute-plugin-hooks-pretooluse-posttooluse-or-is-that-cli-only-right-now/176814
  and
  https://discuss.ai.google.dev/t/stop-and-posttooluse-hooks-in-agents-hooks-json-never-fire-antigravity-ide-1-107-0-windows/178288

## Tools with no capture path

These are detected so the missing attribution is explained, not captured. The
detection signals are conventional install locations (app bundle, `PATH` command,
settings folder, editor extension folder) and have not been checked against each
vendor's documentation. A miss only means no warning.

| Tool | What would unblock capture |
| --- | --- |
| Kiro | Pre/post file-edit hooks with the edited paths, or an extension API that reports agent edits |
| Trae | Same as Kiro |
| Zed (agent panel) | An agent-edit event or hook in Zed's extension/agent API |
| Cline, Roo Code, Kilo Code (VS Code extensions) | Edit hooks in the extension, or a stable on-disk task log with per-edit file paths |
| Aider | A hook around its edit step; today its commits arrive as ordinary git commits |

To ask for an integration, or to contribute one, see
https://autter.dev/docs/cli/add-your-agent.
