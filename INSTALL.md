# Install and set up Autter

Autter is an open source Git extension that records which lines were written by AI agents and links those lines to the agent, model, and prompts behind them. You can use it entirely on your own machine, or connect it to [autter.dev](https://autter.dev) for organization-wide dashboards and collaboration.

Connecting to the platform is optional. The CLI's local authorship tracking, blame, and stats features do not require an Autter account.

## 1. Install the CLI

### Which install to use

Install Autter in the environment where your coding agents and your `git` run.

| Where you work | Use | Notes |
| --- | --- | --- |
| macOS or Linux | the shell installer below | needs `bash` and `curl` |
| Windows, agents started from Windows (PowerShell, Command Prompt, VS Code, Cursor) | the PowerShell installer, or npm if you already have Node.js | run it from a normal terminal, not "Run as administrator" |
| Windows, agents running inside WSL | the shell installer, run inside the WSL terminal | a Windows install does not cover WSL, and the reverse |
| CI | `npm install -g @autter/cli@<version>` or `AUTTER_RELEASE_TAG=v<version>` with the shell installer | the background service never replaces the binary by itself in CI, or for any npm install |
| NixOS, nix-darwin, Home Manager | the [Nix guide](README-nix.md) | |

All of them install the same binary into `~/.autter/bin` (`%USERPROFILE%\.autter\bin` on Windows). Installing only downloads the binary and adds it to your `PATH`. Agent hooks, global git settings and the background service are set up by `autter onboard`, which asks first.

### macOS, Linux, and Windows with WSL

Run:

```bash
curl -fsSL https://api.autter.dev/install.sh | bash
```

Pipe it to `bash`, not `sh`: the script needs bash, and on Debian and Ubuntu `sh` is a different shell. Started with `sh`, it stops and tells you to re-run with `bash`.

To see what the installer would do without downloading, writing or starting anything:

```bash
curl -fsSL https://api.autter.dev/install.sh | bash -s -- --dry-run
```

It lists the download and its checksum check, the binary path, the shell file it would add a line to, the anonymous install ping and the onboarding step. The download is checked against the SHA-256 published with the release before it is installed. If the download fails (a `502`, a proxy, a blocked host), wait a minute and retry, or use npm below.

### Windows (PowerShell, Command Prompt, or Git Bash)

Run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "iex (irm https://api.autter.dev/install.ps1)"
```

Use the `api.autter.dev` URL exactly as shown. Other hosts (such as `autter.dev/install.ps1`) serve the website, and piping that HTML into `iex` fails with PowerShell parse errors.

The installer adds `%USERPROFILE%\.autter\bin` to your user `PATH`. That reaches terminals opened afterwards, not the window you ran the command from: open a new terminal (and restart your IDE) before running `autter`. The installer prints the full path to `autter.exe` at the end so you can run it straight away. If you are already in PowerShell, `irm https://api.autter.dev/install.ps1 | iex` runs the same installer inside your current window, and `autter` works there immediately.

To see what the installer would do without downloading, writing or starting anything, set `AUTTER_INSTALL_DRY_RUN` first. It is an environment variable because `irm | iex` cannot pass arguments:

```powershell
$env:AUTTER_INSTALL_DRY_RUN = '1'
irm https://api.autter.dev/install.ps1 | iex
Remove-Item Env:\AUTTER_INSTALL_DRY_RUN
```

It lists the download and its checksum check, where `autter.exe` goes, the `PATH` change, the Git Bash profile it would add a line to, the anonymous install ping and the onboarding step. Remove the variable afterwards, as shown: while it is set, every run in that window is a dry run. From Command Prompt, run `set AUTTER_INSTALL_DRY_RUN=1` before the `powershell ...` command above.

In a terminal the binary download shows its progress on one line (megabytes received and percent). CI logs and redirected output stay quiet. The download is checked against the SHA-256 published with the release before it is installed. If a download fails (a `502`, a proxy, a blocked host), the error names the URL that failed: wait a minute and retry, or use npm below.

### npm (any platform, Node.js 18+)

```bash
npm install -g @autter/cli
autter onboard
```

The npm package is a thin bootstrapper that downloads the same release binary into `~/.autter/bin` and verifies its checksum against the GitHub release, then exposes `autter` through the npm global bin directory. Hooks and the background service work identically to the script installs; upgrades go through npm (below). One-shot bootstrap without a global install: `npx @autter/cli onboard`.

Three things about the npm route:

- **Run `autter onboard` after installing.** npm hides the output of install scripts, so the hint to do this is not shown. Until onboarding runs, nothing is captured.
- **If install scripts are blocked** (`--ignore-scripts`, pnpm, npm settings that skip them), the binary is not downloaded during `npm install`. It downloads the first time you run `autter`, so no extra flag is needed.
- **npm picks the version.** Upgrade with `npm install -g @autter/cli@latest`, or install `@autter/cli@<version>` to stay on one release. The background service does not replace an npm-installed binary by itself; when a new release is out, autter says so once and names that npm command, and `autter upgrade` prints it instead of installing. `npm update` alone does not upgrade the CLI, because the package only bootstraps the binary. Installing with the shell or PowerShell installer afterwards hands updates back to autter.

> **Git Bash is not WSL.** The bash installer only runs on macOS, Linux, and WSL; in Git Bash it exits with instructions instead of installing. Use the Windows command above (it also works from Git Bash), and install Autter in the environment where your coding agents run — agents inside WSL need the WSL install.

Signed in to the Autter dashboard? [**Settings → CLI setup**](https://app.autter.dev/cli/install) generates a single-use command that runs this same installer and signs the machine in automatically.

The installer downloads Autter into `~/.autter/bin` and adds it to your user `PATH`. IDE/agent hooks, global git capture settings, and the background service are applied during `autter onboard` (with consent) or with `autter install --system`. On macOS, Linux, and WSL the script starts onboarding when the shell is interactive. Automated or non-interactive installs can finish onboarding later with `autter onboard`.

### Where Autter is installed

The install location is fixed and is not configurable. Every install method puts the same binary in the same place:

| | macOS, Linux, WSL | Windows |
| --- | --- | --- |
| Binary | `~/.autter/bin/autter` | `%USERPROFILE%\.autter\bin\autter.exe` |
| `PATH` entry | `~/.autter/env` (and `env.fish`) adds `~/.autter/bin`; one line in your shell file loads it | `%USERPROFILE%\.autter\bin` on your user `PATH` (and one line in `~/.bashrc` for Git Bash) |
| Link | `~/.local/bin/autter`, a symlink to the binary | none |

With npm, the `autter` command in npm's global bin directory is a launcher for that same binary. The installers' dry run prints these paths for your machine.

It is one fixed path because three things point at it: the agent hooks that `autter onboard` installs run the binary by its full path, `autter upgrade` replaces the binary at that path, and the background service runs from it and upgrades it in place. A binary somewhere else would not be upgraded, and hooks would keep running the old one.

If you want `autter` somewhere else, leave the binary where it is and point at it: add a symlink (`ln -s "$HOME/.autter/bin/autter" /usr/local/bin/autter`) or add `~/.autter/bin` to the `PATH` of that environment. Do not move the binary. If you did move it, run `autter install-hooks` so the agent hooks name the path it is at now, then `autter doctor` to check that capture works.

### System requirements

- **git** 2.22 or newer (required)
- **Linux**: glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+, Fedora 36+). Ubuntu 20.04 and older WSL2 distros are not supported natively — use a newer WSL distro or run inside an `ubuntu:22.04` Docker container
- **macOS**: 11 (Big Sur) or newer
- **Windows**: 10 or newer
- **npm path**: Node.js 18+

On macOS, Linux, and WSL, make `autter` available in the terminal you already have open (the installer prints this command at the end too):

```bash
source "$HOME/.autter/env"   # fish: source "$HOME/.autter/env.fish"
```

New terminals pick it up automatically. Restart your IDE (not just its terminal tab) so IDE terminals and coding agents see it too. Then verify the CLI is available:

```bash
autter --version
autter doctor   # v1.7.1+ — focused setup validation (exits 1 on failure)
autter debug    # full support dump (always exits 0)
```

`autter doctor` runs end-to-end checks (git proxy, hooks, checkpoint round-trip). On v1.6.9 and earlier, use `autter debug` instead.

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

To sign in again later (for example after a session expires), run:

```bash
autter login
```

If the browser-based flow does not work, use a Personal Access Token (PAT):

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
- no code, prompts, or agent usage data is uploaded to the Autter platform, even if this machine is signed in.

Choosing a mode does not change system configuration by itself. In a terminal, onboarding asks before it installs IDE/agent hooks, writes global git trace2 settings, or starts the background service. In scripts, add `--yes` to consent to those changes up front (for example `autter onboard --local --yes`); without it a non-interactive run leaves them off, and you can apply them later with `autter install --system`.

Anonymous usage and error telemetry is off until you opt in. Onboarding asks once, in both local and connected mode, and never includes code or prompts. Everything sent is mirrored to `~/.autter/internal/telemetry.log`. Change your answer at any time:

```bash
autter telemetry off
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

In an already-open terminal on macOS, Linux, or WSL, load the PATH entry directly:

```bash
source "$HOME/.autter/env"   # fish: source "$HOME/.autter/env.fish"
```

New terminals load it automatically via your shell configuration. If the command is still not found in a fresh terminal, shell detection could not update your profile — add this line to your shell configuration (for fish, use the `env.fish` variant) and restart the shell:

```bash
. "$HOME/.autter/env"
```

### `autter` is not recognized (Windows PowerShell or Command Prompt)

The installer adds `%USERPROFILE%\.autter\bin` to your user `PATH`, which only reaches terminals opened afterwards. Open a new terminal, or load it into the current PowerShell window:

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','User') + ';' + [Environment]::GetEnvironmentVariable('Path','Machine')
autter --version
```

You can always run it by its full path: `& "$env:USERPROFILE\.autter\bin\autter.exe" doctor`. Restart your IDE as well, so its terminals and agents see the new `PATH`.

### The installer stops with "needs bash" or `Illegal option -o pipefail`

The script was started with `sh`. Run the same command with `bash` at the end (`curl -fsSL https://api.autter.dev/install.sh | bash`). Nothing was installed by the failed run.

### The download fails or returns HTML

A `502` or a timeout is usually temporary: wait a minute and run the installer again, or install with npm (`npm install -g @autter/cli`, then `autter onboard`). Both installers name the URL that failed. If the error mentions HTML or a parse error, check the URL: the installers are served from `api.autter.dev`, not `autter.dev`.

### Windows or WSL: which one to set up

Autter records edits made by agents and commits made by `git` in the environment it is installed in. An agent started from Windows needs the Windows install; an agent running inside WSL needs the install inside WSL. If you use both, install in both and run `autter doctor` in each.

### A warning about running as root or Administrator

Run autter as your normal user. Installed or run with elevated privileges, it creates files your own account cannot open, and the background service then fails with "Access is denied" or "lock held". The warning is printed in full once a day and as one line on other commands. If you do mean to run as root (a container, a build machine), set `AUTTER_ALLOW_SUPERUSER=1`, which silences it. It is already silent in CI.

### `autter doctor` reports timeouts on a slow machine

Doctor's time limits grow with how slowly the machine starts `git` (up to 10 times). On a machine that is slower still, set a scale yourself and re-run: `AUTTER_DOCTOR_TIMEOUT_SCALE=5 autter doctor`.

### `autter doctor` in local mode

Local mode needs no account. Doctor reports sign-in and upload checks as skipped there, not as warnings. To sync to the Autter dashboard, run `autter onboard --connect`.

### Remove Autter's setup

```bash
autter uninstall --dry-run   # lists what would be removed
autter uninstall             # asks first; --yes for scripts
```

This removes Autter's agent hooks and editor extensions, its settings in your global git `trace2` config (your own trace2 settings stay) and stops the background service. The binary and the `PATH` entry are left in place.

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

### Attribution checks fail (`autter doctor` / `autter debug`)

`autter doctor` prints a `fix:` line with a likely cause and next command for each failure. Common cases:

1. **Checkpoints not persisting** (timeout waiting for working-log entries):  
   `autter bg status`, then `autter bg restart`, then `autter doctor`.
2. **Trace2 config / event capture failed**:  
   `autter install`, then `autter doctor`. Attribution depends on the git proxy and trace2 settings.
3. **Commit succeeded but blame lines are all untracked / wrong class**:  
   post-commit notes did not land — usually the same as (2). Run `autter install` and ensure commits go through the Autter-managed `git`.
4. **Still stuck**: run `autter debug` and review (or share) its attribution self-check section, including the leftover self-check repo path if one was kept for inspection.

When `autter diff` / `blame` / `show` report missing authorship on real commits, the same sequence applies: hooks (`install-hooks`) → install → doctor.

For additional diagnostics, run `autter debug` and review its output before including it in an issue.

What the background service is, what it listens to and how to inspect it: [docs/background-service.md](docs/background-service.md).
