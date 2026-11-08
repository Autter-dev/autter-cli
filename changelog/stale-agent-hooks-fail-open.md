# Out-of-Date Agent Hooks Never Block the Agent, Refresh After an Upgrade, and Fail `doctor`

## Summary

A tester's Google Antigravity agent was blocked by autter's hook with no message, while `autter doctor` showed only a warning ("hooks installed but out of date").

Antigravity reads every hook's stdout as JSON. The checkpoint handler printed `{}` for it, but only after the background service check and argument parsing had passed, and several hook paths still exited non-zero. Replayed against the 3.2.2 binary:

| Hook invocation | Exit | stdout |
| --- | --- | --- |
| `checkpoint antigravity-pre --hook-input stdin`, background service unreachable (e.g. restarting after an upgrade) | 0 | empty (no JSON) |
| `checkpoint <preset this CLI doesn't know> --hook-input stdin` | 1 | empty |
| `checkpoint antigravity-pre --hook-input` (value missing) | 1 | `{}` |
| `memory-hook <agent> --hook-input stdin`, background service unreachable | 1 | empty |

Hooks were rewritten only by `autter install-hooks`, so a hook could keep calling a binary that had moved or a preset an older CLI used.

## Changes

### Hooks fail open (`src/commands/autter_handlers.rs`)

- Agents that parse hook output get their reply before anything else can fail. Any preset starting with `antigravity`, and any unknown preset sent by a hook, gets `{}` first; `{}` is a no-op for every agent that parses hook output.
- An invocation carrying `--hook-input` (or `AUTTER_CHECKPOINT_HOOK`) that this CLI can't run as written (unknown preset, `--hook-input` with no value, a flag error) exits 0 and prints ``autter hook is out of date. Run `autter install-hooks` to refresh it (<reason>).`` on stderr. Run by hand, an unknown preset still exits 1.
- `memory-hook` exits 0 when the background service is unreachable, as `checkpoint` already did.
- A panic inside a hook checkpoint is still reported, and the hook exits 0.
- Audit: Claude Code, Codex, Gemini CLI, Cursor, Windsurf, Droid, GitHub Copilot and Firebender run autter as a shell command and are covered by the tests below. Amp, OpenCode and Pi call autter from plugin code that already catches failures.

### Refresh after an upgrade (`src/mdm/hook_refresh.rs`)

- `~/.autter/internal/hooks_version.json` records the CLI version that last installed or refreshed hooks, and which agents failed.
- The first `autter` command after the version changes starts `autter install-hooks --refresh-stale` in the background (checking every agent can take seconds, too long inside a hook). It rewrites hooks only for agents whose autter hooks are installed and out of date. Agents without autter hooks (never set up, or declined) are not touched, and editor extensions (VS Code, JetBrains, Visual Studio) are left to `autter install-hooks`.
- `autter upgrade` runs the refresh with the newly installed binary on macOS and Linux. On Windows the installer finishes in the background, so the first command afterwards does it.
- A full `autter install-hooks` updates the marker too. Hook install errors now count as `failed` in its statuses (they were reported as `not_found`).

### Doctor (`src/commands/doctor.rs`)

- Out-of-date hooks are **Failed** when a hook command fails every time the agent runs it (the binary it names is gone, or its preset is unknown) or when the automatic refresh could not rewrite them. The check names the agent and lists each failing command exactly, with the fix. Hooks that still run are a **Warning**.
- Doctor compares hooks against the same binary path `install-hooks` writes (`resolve_hook_binary_path`), so a symlinked install is not reported as out of date.
- Installers expose `hook_config_paths()` so doctor can read the exact commands (Claude Code, Codex, Cursor, Gemini CLI, Droid, Firebender, Windsurf, GitHub Copilot, Antigravity).

### Antigravity installer

- Hook commands use forward slashes for Windows binary paths, as every other agent's hooks do.

## Tests

- `tests/integration/stale_hooks.rs`: for every shell-hook preset, an unknown preset, a dropped `--hook-input` value, an unreachable background service and garbage input all exit 0 with valid stdout (`{}` for Antigravity) and the out-of-date message where relevant. An end-to-end upgrade test sets an old version marker and a stale Antigravity hook, checks that doctor fails with the command, runs one command, and checks that the hook is refreshed, the user's own hook is kept, no hooks are added for Claude Code or Gemini, and doctor passes.
- `src/mdm/hook_refresh.rs`: only stale, opted-in agents are refreshed; failing vs cosmetic command classification; command extraction from JSON and Codex TOML.
- Doctor snapshots for failing, could-not-refresh and still-runs cases.
