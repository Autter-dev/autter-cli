# Install and set up Autter

Autter is an open source Git extension that records which lines were written by AI agents and links those lines to the agent, model, and prompts behind them. You can use it entirely on your own machine, or connect it to [autter.dev](https://autter.dev) for organization-wide dashboards and collaboration.

Connecting to the platform is optional. The CLI's local authorship tracking, blame, and stats features do not require an Autter account.

## 1. Install the CLI

### macOS, Linux, and Windows with WSL

Run:

```bash
curl -sSL https://autter.dev/install.sh | bash
```

### Windows (PowerShell)

Open PowerShell and run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://autter.dev/install.ps1 | iex"
```

The installer downloads Autter into `~/.autter/bin`, adds it to your user `PATH`, configures supported coding agents and editors, and starts the background service. On macOS, Linux, and WSL it then starts onboarding when the shell is interactive. Automated or non-interactive installs can finish onboarding later.

Close and reopen your terminal and IDE after installation, then verify the CLI is available:

```bash
autter --version
autter debug
```

You do not need to configure each repository separately. Continue using Git, your IDE, and your coding agents as usual.

> Using Nix? See the [Nix installation guide](README-nix.md) for NixOS, nix-darwin, and Home Manager options.

## 2. Choose how to use Autter

Run the guided setup at any time:

```bash
autter onboard
```

The setup offers two modes.

### Connect to the Autter platform

Connecting this machine makes the local attribution useful beyond one laptop or repository. It lets you:

- search and inspect the prompt and model behind an AI-generated change;
- see AI and human authorship across repositories, contributors, and teams;
- understand which agents, models, and workflows are effective;
- follow AI-generated code through review and into production; and
- keep organization-wide audit history and usage trends in one place.

Local Git notes remain available when connected. Prompt and usage data also syncs to your organization's Autter environment. See [Data Privacy](data-privacy.md) for exactly what is stored in each mode.

The simplest login method is the guided browser flow:

```bash
autter onboard
```

Choose to connect when prompted. Autter opens a browser, shows a one-time code, and waits for you to authorize the device. To select connected mode without the initial question, run:

```bash
autter onboard --connect
```

If the browser-based onboarding flow does not work, use a Personal Access Token (PAT):

1. Sign in to the Autter web app.
2. Open the organization you want to connect.
3. Go to **Org Settings → Access Tokens**.
4. Create and copy a Personal Access Token.
5. Add the token to the CLI:

   ```bash
   autter login --token <paste-your-token>
   ```

Treat a PAT like a password: do not commit it, paste it into chat, or include it in shell scripts. After either login method, confirm the active identity and organization:

```bash
autter whoami
```

If you belong to more than one organization, Autter uses the repository's Git remote to route connected data to the organization that owns it, with your default organization as the fallback.

### Use Autter locally without logging in

An account is not required. Choose local-only during `autter onboard`, or select it directly:

```bash
autter onboard --local
```

In local-only mode:

- line-level attribution is stored in local Git notes under `refs/notes/ai`;
- prompts remain in local storage on your machine;
- local commands such as `autter blame` and `autter stats` continue to work; and
- no code, prompts, or agent usage data is uploaded to the Autter platform.

Open source error and exception telemetry is enabled by default. If you want Autter to send no telemetry at all, turn it off:

```bash
autter config set telemetry_oss off
```

You can connect later without reinstalling:

```bash
autter onboard --connect
```

To disconnect an existing installation and return to local-only mode:

```bash
autter logout
autter onboard --local --force
```

## 3. Use Autter

There is no new commit workflow to learn. Ask your supported agent to edit code, review the changes, and commit normally:

```bash
git add .
git commit -m "Describe the change"
```

Autter attaches authorship metadata to the commit automatically. Useful commands include:

```bash
# See line-by-line human and AI authorship
autter blame path/to/file

# Summarize authorship over a commit or range
autter stats
autter stats <start-sha>..<end-sha>

# Check the current platform login
autter whoami

# Re-run agent/editor integration setup
autter install-hooks

# Upgrade to the newest release
autter upgrade
```

Run `autter help` for the full command list.

## Open source, extensible, and local-first

The CLI and the [Autter authorship standard](specs/autter_standard_v3.0.0.md) are open source. The platform is an optional collaboration layer, not a requirement for attribution. You can build and run your own solution on the standard, keep it local, or connect Autter to the tools your organization already uses.

Community contributions are welcome, including:

- support for new coding agents and checkpoint formats;
- editor and IDE plugins that display authorship and prompt context;
- CI integrations and workflows;
- storage, reporting, and self-hosting integrations; and
- fixes and improvements to the CLI and open standard.

Start with the [contribution guide](CONTRIBUTING.md), read the [agent integration guide](https://autter.dev/docs/guides/add-your-agent), or open a [pull request](https://github.com/autter-dev/autter-cli/pulls). Existing community editor plugins are listed in the [main README](README.md#ai-blame).

## Troubleshooting

### `autter: command not found`

Close and reopen the terminal so the installer-added `PATH` entry is loaded. If shell detection could not update your profile on macOS, Linux, or WSL, add this line to your shell configuration and restart the shell:

```bash
export PATH="$HOME/.autter/bin:$PATH"
```

### An agent or editor installed after Autter is not detected

Re-run integration setup:

```bash
autter install-hooks
```

### Browser onboarding fails

Use the PAT fallback described above:

```bash
autter login --token <paste-your-token>
autter whoami
```

### Change the original onboarding choice

```bash
autter onboard --force
```

For additional diagnostics, run `autter debug` and review its output before including it in an issue.
