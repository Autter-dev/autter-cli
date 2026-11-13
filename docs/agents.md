# Supported agents and editors

autter attributes a line to an AI agent when the agent tells autter, right before
and right after it edits a file. Each agent below does that in one of three ways:

- **CLI hooks**: `autter install-hooks` adds hook commands to the agent's own
  config file. They run `autter checkpoint <preset> --hook-input stdin`.
- **Plugin**: `autter install-hooks` writes a plugin file the agent loads, and the
  plugin runs `autter checkpoint`.
- **IDE extension**: an editor extension or IDE plugin watches edits and sends the
  checkpoints itself.

`autter install-hooks` (or `autter onboard`) sets up every agent it finds on the
machine. To check one agent, run its `autter doctor --agent` command. It reports
whether the agent is detected, which files autter manages for it, whether each
hook command runs, and then sends the agent's own pre- and post-edit hook calls
through autter in a throwaway repository. It passes when the background service
records an AI checkpoint for the edited file, so no real agent session is needed.
Every failed step prints a `fix:` line.

| Agent | Integration | What is captured | What is not | Check it |
| --- | --- | --- | --- | --- |
| Claude Code | CLI hooks in `~/.claude/settings.json` (and Claude Agent SDK harness settings) | Every tool edit (Write, Edit, MultiEdit, any tool that names a file) and Bash commands; model and prompts from the transcript | Claude Code older than 2.0 | `autter doctor --agent claude-code` |
| Codex | CLI hooks in `~/.codex/config.toml` (or `~/.codex/hooks.json`) | `apply_patch` edits and shell commands; prompts from the rollout file | The model when Codex's hook does not send it (recorded as unknown) | `autter doctor --agent codex` |
| Cursor | CLI hooks in `~/.cursor/hooks.json`, plus the autter extension | Write, StrReplace and Delete edits and Shell commands from the agent; model from the hook or transcript | Tab completions (only with the extension's experimental AI-tab tracking); Cursor older than 1.7 | `autter doctor --agent cursor` |
| GitHub Copilot (VS Code agent mode and Copilot CLI) | CLI hooks in `~/.copilot/hooks/autter.json` | File edits and terminal commands in agent mode and the Copilot CLI; model from the chat session | Inline completions and inline chat; VS Code older than 1.99 | `autter doctor --agent github-copilot` |
| Amp | Plugin `~/.config/amp/plugins/autter.ts` | Edit and Write tool calls and Bash commands that finish; model from the thread | Failed or cancelled tool calls | `autter doctor --agent amp` |
| OpenCode | Plugin `~/.config/opencode/plugins/autter.ts` | edit, write, patch, multiedit and apply_patch, plus bash; model from OpenCode's store | Other tools | `autter doctor --agent opencode` |
| Pi | Plugin `~/.pi/agent/extensions/autter.ts` | edit and write tool calls and bash (more tools through `~/.pi/agent/autter.override.json`); model from the session file | Tools not in the policy | `autter doctor --agent pi` |
| Gemini CLI | CLI hooks in `~/.gemini/settings.json` | write_file and replace edits and shell commands; model from the transcript | | `autter doctor --agent gemini` |
| Droid (Factory) | CLI hooks in `~/.factory/settings.json` | ApplyPatch, Edit, Write and Create edits and Bash commands | | `autter doctor --agent droid` |
| Firebender | CLI hooks in `~/.firebender/hooks.json` | Write, Edit, Delete and symbol edits and Bash commands; model from the hook | | `autter doctor --agent firebender` |
| Google Antigravity | CLI hooks in `~/.gemini/config/hooks.json` | File writes and `run_command` in the Antigravity CLI and 2.0 app | The Antigravity IDE has been reported not to run hooks ([details](unsupported-agents.md#google-antigravity)) | `autter doctor --agent antigravity` |
| Kiro | CLI hooks in `~/.kiro/hooks/autter.json` (Kiro CLI v3) | File writes and shell commands from the Kiro CLI agent | Kiro CLI 2.x; the Kiro IDE unless it loads `~/.kiro/hooks/`; the model and prompts. Payload fields not yet confirmed ([details](unsupported-agents.md#kiro)) | `autter doctor --agent kiro` |
| Windsurf | CLI hooks in `~/.codeium/windsurf/hooks.json` (and `~/.codeium/hooks.json`), plus the autter extension | Cascade file writes and commands | Tab completions | `autter doctor --agent windsurf` |
| VS Code | IDE extension `autter.autter-vscode` | Your own typing (known human); Copilot edits on VS Code older than 1.109.3, where native hooks are missing | Inline chat; AI tab completions unless the experimental setting is on | `autter doctor --agent vscode` |
| JetBrains IDEs | IDE plugin (build 252 or newer) | Edits by GitHub Copilot for JetBrains and Junie | The model and prompts; other JetBrains AI plugins | `autter doctor --agent jetbrains` |
| Visual Studio (Windows) | IDE extension (VSIX, Visual Studio 2022) | Copilot edits | The model and prompts | `autter doctor --agent visual-studio` |

Shell commands an agent runs (Bash, `run_in_terminal`, ...) are captured by
comparing the files before and after the command. Gitignored files and deletions
are not attributed; see [shell-edit-capture.md](shell-edit-capture.md).

For an editor extension there is no hook call to simulate:
`autter doctor --agent` checks that the extension is installed, and the simulated
edit is skipped. To check one, edit a file in the editor, then run `autter status`.

Agents not in this table have no autter integration yet. Their edits show as
untracked in `autter stats` and as yours in `autter blame`;
[unsupported-agents.md](unsupported-agents.md) lists the ones `autter doctor`
recognises and what is missing. To add an agent, see the
[agent integration guide](https://autter.dev/docs/guides/add-your-agent).
