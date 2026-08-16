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

## Environment variables

- `AUTTER_NPM_SKIP_DOWNLOAD=1` — skip the postinstall binary download (it happens on first run instead).
- `AUTTER_RELEASE_TAG=vX.Y.Z` — install a specific release.
- `AUTTER_NO_INSTALL_PING=1` — disable the anonymous install-count ping (OS, architecture, and version only).

## Links

- [Documentation](https://docs.autter.dev)
- [GitHub repository](https://github.com/Autter-dev/autter-cli)
- [Other install methods](https://docs.autter.dev/cli/install) (`curl`, PowerShell, Nix)
