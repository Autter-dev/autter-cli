# Windows Release Binaries & Installer Fixes

## Summary

Windows installs were broken end-to-end, with five independent reports converging on the same experience: the documented PowerShell one-liner either died in the shell before running, crashed inside architecture detection, or failed with an opaque `Failed to download binary (HTTP error)`. Three stacked root causes:

1. **Releases shipped no Windows binaries at all.** `release.yml` had no Windows targets in its build matrix, so every `autter-windows-x64.exe` download URL 404'd — while the docs advertised Windows support.
2. **`install.ps1` architecture detection could crash.** `Get-Architecture` probed `RuntimeInformation::OSArchitecture` (unresolvable on some Windows PowerShell 5.1 hosts) and its env-var fallback misdetected 32-bit shells on 64-bit Windows (`PROCESSOR_ARCHITECTURE='x86'` under WOW64), returning `$null` — and the "unsupported architecture" error message then re-probed `RuntimeInformation` *outside* any try/catch, replacing the friendly error with an uncaught `PropertyNotFound`/`TypeNotFound` exception.
3. **Download failures were opaque.** All download exceptions were swallowed and reported as a single `Failed to download binary (HTTP error)` with no URL, status code, or troubleshooting path.

Fixing this also surfaced a latent release bug affecting **all** platforms: the version-pinned install scripts attached to releases silently ignored their version pin and skipped checksum verification (see below).

## Changes

### `.github/workflows/release.yml`

- **New build targets**: `autter-windows-x64.exe` (`x86_64-pc-windows-msvc`) and `autter-windows-arm64.exe` (`aarch64-pc-windows-msvc`, cross-compiled on the same x64 runner via the VS 2022 ARM64 toolchain), both on `windows-2022`. No OpenSSL is needed on Windows — TLS goes through native-tls/SChannel, and the only C build is the bundled SQLite.
- The stage step appends `.exe` to the built-binary path for Windows targets; the assemble step's asset list (single `assets` variable now, previously duplicated) includes both Windows binaries in `checksums.txt` and the embedded checksum string.
- **Version-pinned `install.ps1` is now attached to releases** (filled by the same `fill-install-template.py`), giving Windows the pin + checksum verification that install.sh already had on paper.

### `install.ps1`

- **`Get-Architecture` rewritten**: checks `PROCESSOR_ARCHITEW6432` first (set for 32-bit shells on 64-bit Windows, where `PROCESSOR_ARCHITECTURE` misreports `x86`), then `PROCESSOR_ARCHITECTURE`, then falls back to `RuntimeInformation` inside a try/catch. Env vars exist on every PowerShell version and can't throw.
- **Unsupported-architecture error no longer crashes**: it reports `$env:PROCESSOR_ARCHITECTURE` instead of re-probing `RuntimeInformation` outside a try/catch.
- **Transparent download errors**: each failed attempt is recorded as `<url> -> HTTP <code> <status>` (or the exception message for network/TLS failures). The final error lists every attempted URL with its specific failure, and a 404 gets targeted guidance: the release has no Windows binary, Windows binaries ship with v1.6.8+, and how to unpin `AUTTER_RELEASE_TAG`.

### `install.sh` + `install.ps1` — fill-proof placeholder guards

`fill-install-template.py` blindly replaces every occurrence of each placeholder token, **including the guard comparisons**. In the pinned copy attached to releases, `[ "$PINNED_VERSION" != "__VERSION_PLACEHOLDER__" ]` became `[ "v1.6.7" != "v1.6.7" ]` (never true → the pin was ignored and "latest" installed) and the checksum guard compared the checksums string to itself (always true → verification silently skipped). Verified against the actual v1.6.7 release asset. Both scripts now compare against sentinel values built by string concatenation (`'__VERSION_' + 'PLACEHOLDER__'`), which survive the fill; comments no longer embed the literal tokens either.

### `README.md`, `INSTALL.md`

- The Windows one-liner is now `powershell ... -Command "iex (irm https://api.autter.dev/install.ps1)"` instead of `"irm ... | iex"`. With the pipe form, any context that strips/mangles the quotes (smart-quote rendering, copy-paste through quote-stripping shells) turns `|` into a real shell pipe and `iex` is lost, producing "'iex' is not recognized". The `iex (...)` form has no pipe to hijack and survives quote stripping in cmd, PowerShell, and Git Bash.

## Behaviour Guarantees

| Scenario | Before | After |
|---|---|---|
| Windows install, any release ≤ v1.6.7 | 404 → `Failed to download binary (HTTP error)` | v1.6.8+ releases include `autter-windows-{x64,arm64}.exe`; 404s explain the cause and fix |
| 32-bit PowerShell on 64-bit Windows | arch `$null` → uncaught `PropertyNotFound`/`TypeNotFound` crash | detected via `PROCESSOR_ARCHITEW6432` → correct binary |
| PS 5.1 host without `RuntimeInformation` | crash or fallback-dependent | env-var detection, no type probing needed |
| Download failure (network/proxy/TLS) | opaque one-liner | per-URL reason + troubleshooting hints |
| Pinned release install script | installed "latest", skipped checksum verification | installs the pinned version, verifies checksums |
| One-liner pasted through quote-stripping contexts | `'iex' is not recognized` | works (no pipe to hijack) |
