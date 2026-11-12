# Capturing Kiro hook payloads

autter's Kiro support ([unsupported-agents.md](unsupported-agents.md#kiro)) is built
from Kiro's documentation, not from real hook input. Kiro documents the hook
envelope (`hook_event_name`, `cwd`, `session_id`, `tool_name`, `tool_input`,
`tool_response`) but not which `tool_input` field holds the edited file, what the
CLI v3 tool names are, or whether a model is sent. A few real payloads settle all
of that. This page shows how to record them in about ten minutes.

The capture hook only appends what Kiro sends to a file in your home folder. It
prints nothing and always exits 0, so it never blocks or changes what the agent
does.

## 1. Save the capture script

macOS and Linux: save this as `~/kiro-capture.sh` and run
`chmod +x ~/kiro-capture.sh`.

```sh
#!/bin/sh
# Appends one Kiro hook call to ~/kiro-hook-capture.log. Prints nothing, exits 0.
out="$HOME/kiro-hook-capture.log"
{
  echo "=== $(date -u +%Y-%m-%dT%H:%M:%SZ) label=$1"
  # Only these variables, so no tokens or keys from your environment are copied.
  env | grep -E '^(KIRO[A-Z_]*|TERM_PROGRAM|TERM_PROGRAM_VERSION|VSCODE_PID|PWD)=' | sort
  echo "--- stdin"
  cat
  echo
} >> "$out" 2>/dev/null
exit 0
```

Windows: save this as `%USERPROFILE%\kiro-capture.ps1`.

```powershell
# Appends one Kiro hook call to ~\kiro-hook-capture.log. Prints nothing, exits 0.
$out = Join-Path $HOME 'kiro-hook-capture.log'
try {
  $stdin = [Console]::In.ReadToEnd()
  $vars = Get-ChildItem env: | Where-Object { $_.Name -match '^(KIRO.*|TERM_PROGRAM.*|VSCODE_PID)$' } |
    ForEach-Object { "$($_.Name)=$($_.Value)" }
  $text = @("=== $((Get-Date).ToUniversalTime().ToString('s'))Z label=$($args[0])") + $vars +
    @("PWD=$PWD", '--- stdin', $stdin, '')
  Add-Content -Path $out -Value $text
} catch {}
exit 0
```

## 2. Add the capture hooks

Save this as `~/.kiro/hooks/autter-capture.json`. Replace `/Users/YOU` with your
home folder (on Windows use
`powershell -NoProfile -File C:/Users/YOU/kiro-capture.ps1 PreToolUse` and so on).
These hooks have no `matcher`, so they see every tool, not just writes.

```json
{
  "version": "v1",
  "hooks": [
    {"name": "capture: PreToolUse", "trigger": "PreToolUse", "enabled": true, "timeout": 10,
     "action": {"type": "command", "command": "/Users/YOU/kiro-capture.sh PreToolUse"}},
    {"name": "capture: PostToolUse", "trigger": "PostToolUse", "enabled": true, "timeout": 10,
     "action": {"type": "command", "command": "/Users/YOU/kiro-capture.sh PostToolUse"}},
    {"name": "capture: UserPromptSubmit", "trigger": "UserPromptSubmit", "enabled": true, "timeout": 10,
     "action": {"type": "command", "command": "/Users/YOU/kiro-capture.sh UserPromptSubmit"}}
  ]
}
```

For the Kiro IDE, also save `~/.kiro/hooks/autter-capture-ide.json`. It is a
separate file so that, if the CLI rejects the IDE-only trigger, the hooks above
still load.

```json
{
  "version": "v1",
  "hooks": [
    {"name": "capture: PostFileSave", "trigger": "PostFileSave", "enabled": true, "timeout": 10,
     "action": {"type": "command", "command": "/Users/YOU/kiro-capture.sh PostFileSave"}}
  ]
}
```

If you use autter, keep `~/.kiro/hooks/autter.json` where it is. The capture hooks
run alongside it.

## 3. Make the edits

Do this in a throwaway git repository (for example `git init /tmp/kiro-test`).
Run each step in the **Kiro CLI (v3)** and again in the **Kiro IDE**, then restart
the IDE if the capture log stays empty.

1. **Vibe edit:** ask the agent to "add a function `hello()` to `notes.py`".
   Then ask it to change one line in that function, so both creating and editing a
   file are recorded.
2. **Spec edit:** start a spec ("create a spec for a CLI that greets people") and
   let it carry out at least one task that writes code.
3. **Shell edit:** ask the agent to "run `echo done >> notes.py`", so a file is
   changed by a shell command rather than a write tool.

Also note:

- your Kiro CLI version (`kiro-cli --version`) and your Kiro IDE version
  (Help, About)
- which model was selected in each
- whether the IDE wrote anything to the log at all

## 4. Send it back

Open `~/kiro-hook-capture.log` and check it for anything private (file contents,
paths or prompts you don't want to share), then send the log with:

- the versions and models listed above
- your operating system

Attach it to an issue on the autter CLI repository titled "Kiro hook capture".

## 5. Remove the capture hooks

```sh
rm ~/.kiro/hooks/autter-capture.json ~/.kiro/hooks/autter-capture-ide.json ~/kiro-capture.sh
```

The log stays in your home folder until you delete it (`rm ~/kiro-hook-capture.log`).

## What the capture answers

| Question | Where it shows up in the log |
| --- | --- |
| Which field names the edited file | `tool_input` of the PreToolUse and PostToolUse entries for the write tool |
| The CLI v3 and IDE tool names | `tool_name` |
| Whether a model or tool-call id is sent | any `model…` or `tool_use_id`-like field |
| Whether the IDE loads `~/.kiro/hooks/` | any entry recorded while you used the IDE |
| How Kiro launches hooks | the `KIRO_*`, `TERM_PROGRAM` and `VSCODE_PID` lines |
