# autter Extension for VS Code & Cursor

A VS Code and Cursor extension that tracks AI-generated code using [autter](https://github.com/autter-dev/autter-cli?tab=readme-ov-file#quick-start).

## Manual Install

The [autter quickstart](https://github.com/autter-dev/autter-cli?tab=readme-ov-file#quick-start) install script should automatically install both the Cursor and VS Code extensions automatically, however, if that didn't work or you'd like to install the extension manually, please follow the instructions below:

### Cursor

1. **Install the extension** We recommend installing the Cursor extension by searching for `autter` in the extensions tab. To open the extensions tab, you can press `Cmd+Shift+P` and search for `>Extensions: Install Extensions`.
2. **Install [`autter`](https://github.com/autter-dev/autter-cli)** Follow the `autter` installation [instructions](https://github.com/autter-dev/autter-cli?tab=readme-ov-file#quick-start) for your platform.
3. **Restart VS Code**

### VS Code

1. **Install the extension** We recommend installing from the [VS Code Extension marketplace](https://marketplace.visualstudio.com/items?itemName=autter.autter-vscode)
2. **Install [`autter`](https://github.com/autter-dev/autter-cli)** Follow the `autter` installation [instructions](https://github.com/autter-dev/autter-cli?tab=readme-ov-file#quick-start) for your platform.
3. **Restart VS Code**

### Debug logging

You can enable toast messages from the extension when it calls checkpoints to get a feel for the effectiveness of the heuristics add this option to your settings:

```json
"autter.enableCheckpointLogging": true
```

### Review findings in the editor

When the branch you have checked out has an open pull request that Autter reviewed, its findings show up next to the code: squiggles on the affected lines, entries in the Problems panel, and (by default) the finding's title after the line. Quick fixes on each finding copy a fix prompt for your AI agent, open the finding in the dashboard, or dismiss it.

Findings are only shown on lines the review saw. Line numbers follow your edits: lines added or removed above a finding move it, and a finding whose own line you've edited since the review is no longer shown in the code (it's listed in the findings file as possibly fixed).

The extension keeps `.autter/findings.md` up to date with every finding: a summary, its location, next steps, and a link. `.autter/` is added to `.git/info/exclude`, so it is never committed.

- **Sign in**: click **autter: Sign in** in the status bar. It runs `autter login` in a terminal.
- **Settings**: `autter.findings.display` (`inline`, `problems` or `off`; default `inline`) and `autter.findings.writeFile` (default `true`).
- **Commands**: *Autter: Refresh Review Findings*, *Autter: Open Review Findings File*, *Autter: Show Dismissed Review Findings Again*.
- Requires an `autter` CLI with the `autter findings` command.

### AI tab tracking (experimental)

Adds support for tracking AI tab-completion insertions

- **Setting**: `autter.experiments.aiTabTracking` (default: `false`)
- **Notes**:
  - Requires restarting VS Code/Cursor after changing the setting.
  - On activation, you'll see a notification: "autter: AI tab tracking is enabled (experimental)".
  - Requires the latest release of the `autter` CLI

Enable it in Settings or add to your `settings.json`:

```json
"autter.experiments.aiTabTracking": true
```

## License

MIT
