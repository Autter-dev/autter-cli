# @autter/cli

**[Autter](https://autter.dev) is an open source git extension that records which lines of your code were written by AI** — tied to the agent, model, and prompt that produced them.

This package is a thin bootstrapper: it downloads the official native `autter` binary for your platform (macOS, Linux, or Windows; x64 or arm64) into `~/.autter/bin`, verifies its checksum against the GitHub release, and exposes the `autter` command through your npm global bin. The binary location is shared with the [shell installers](https://docs.autter.dev/cli/install), so hooks and the background service's self-updates keep working no matter how autter was installed.

## Install

```bash
npm install -g @autter/cli
autter onboard
```

Or bootstrap without a global install:

```bash
npx @autter/cli onboard
```

The package version matches the CLI release, so `npm install -g @autter/cli@<version>` installs that exact release.

Run `autter onboard` after installing: npm hides the output of install scripts, so nothing on screen tells you to, and nothing is captured until onboarding has run.

If your package manager skips install scripts (`--ignore-scripts`, pnpm, or an npm setting), the binary is downloaded the first time you run `autter` instead. No extra flag is needed.

## Upgrade and pin

- npm picks the version. Upgrade with `npm install -g @autter/cli@latest`, or install `@autter/cli@<version>` to stay on that release. `npm update` alone does not upgrade the CLI, because this package only bootstraps the binary.
- The background service does not replace an npm-installed binary by itself. When a newer release is out, autter tells you once and names the npm command; `autter upgrade` prints the same command instead of installing.
- The package records this in `~/.autter/npm-install.json`. Installing afterwards with the shell or PowerShell installer removes that file and hands updates back to autter.

## Environment variables

- `AUTTER_NPM_SKIP_DOWNLOAD=1` — skip the postinstall binary download (it happens on first run instead).
- `AUTTER_RELEASE_TAG=vX.Y.Z` — install a specific release.
- `AUTTER_NO_INSTALL_PING=1` — disable the anonymous install-count ping (OS, architecture, and version only).
- `AUTTER_DISABLE_AUTO_UPDATES=1` — stop the background service from upgrading the binary by itself.

## Links

- [Documentation](https://docs.autter.dev)
- [GitHub repository](https://github.com/Autter-dev/autter-cli)
- [Other install methods](https://docs.autter.dev/cli/install) (`curl`, PowerShell, Nix)
