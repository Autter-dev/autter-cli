# Quiet Installer Output

## Summary

Fresh installs printed a wall of scary-but-harmless noise: three red `Extension 'autter.autter-vscode' not found` failures (with a Node `url.parse()` deprecation warning) before the Open VSX `.vsix` fallback succeeded, and literal `-e` prefixes on the closing messages when the script was piped to `sh` instead of `bash`. This change makes the extension install quiet and fail-fast, and makes `install.sh` output shell-agnostic.

Root cause of the extension errors: `autter.autter-vscode` is published on Open VSX but has no Microsoft Marketplace listing, so the ID-based install VS Code and Cursor perform always fails and every install rides the `.vsix` fallback. (Publishing to the MS Marketplace remains the long-term fix.)

## Changes

### `src/mdm/utils.rs`

- **`install_vsc_editor_extension`** now captures the editor CLI's stdout/stderr (`.output()`) instead of inheriting the terminal (`.status()`). Raw editor output — red "not found" errors, `DEP0169 url.parse()` warnings — no longer reaches the user; it is logged via `tracing::debug!` and folded into the returned error message. The caller's clean one-line status (`✓ VS Code: Extension installed`) is now the only thing users see on success.
- **Fail-fast on deterministic errors**: an "extension not found" response means the editor's marketplace doesn't carry the extension — retrying can't help. The 3-attempt retry loop (kept for genuinely flaky editor-CLI JS errors) now returns immediately on "not found", so the Open VSX `.vsix` fallback engages without two pointless retries. Benefits the VS Code, Cursor, and Windsurf installers, which all share this path.

### `src/mdm/agents/vscode.rs`

- The manual-install fallback message pointed to a Microsoft Marketplace listing that doesn't exist. It now directs users to download the `.vsix` from `https://open-vsx.org/extension/autter/autter-vscode` and install it with `code --install-extension <file>`.

### `install.sh`

- All `echo -e` calls replaced with `printf '%b\n'`. When the script is run via `curl … | sh`, the shebang is bypassed and POSIX-mode shells (dash, macOS `/bin/sh`) print `echo -e`'s `-e` literally — users saw `-e ✓ /Users/…/.zshrc` and `-e Close and reopen your terminal…`. `printf '%b\n'` renders colors and escapes identically under bash, zsh, dash, and macOS `sh`.

## Behaviour Guarantees

| Scenario | Before | After |
|---|---|---|
| Extension not on editor's marketplace | 3 red failures + deprecation warning printed, then `.vsix` fallback | silent fail-fast, `.vsix` fallback immediately, single `✓` line |
| Extension install flaky JS error | retried up to 3×, raw output shown | retried up to 3×, output captured to debug log |
| Both marketplace and Open VSX fail | error pointed to nonexistent MS Marketplace listing | error points to Open VSX manual `.vsix` install |
| `curl … \| sh` install | literal `-e` prefixes in output | clean colored output |
