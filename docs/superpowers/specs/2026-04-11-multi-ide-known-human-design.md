# Multi-IDE Known-Human Checkpoint — Design Spec

**Date:** 2026-04-11  
**Status:** Approved  

---

## Background

PR #1023 introduced the `known_human` checkpoint: when a user saves a file in an IDE, the
extension fires `autter checkpoint known_human --hook-input stdin` with a JSON payload containing
the saved file paths and content. This lets autter attribute those lines to the human author
rather than leaving them as unknown after an AI edit session.

VS Code (via its extension, `agent-support/vscode/`) and JetBrains IDEs (via the IntelliJ
plugin, `agent-support/intellij/`) already implement this. Cursor reuses the VS Code extension.

This spec covers adding the same capability to 9 additional IDE/editor targets, one PR each,
with subagents running in parallel git worktrees.

---

## Checkpoint Protocol (unchanged)

Every integration must call:

```
autter checkpoint known_human --hook-input stdin
```

with the following JSON written to stdin:

```json
{
  "editor": "<editor-id>",
  "editor_version": "<version string>",
  "extension_version": "<plugin/extension version>",
  "cwd": "/absolute/path/to/git/repo/root",
  "edited_filepaths": ["/abs/path/file1.ext", "/abs/path/file2.ext"],
  "dirty_files": {
    "/abs/path/file1.ext": "<full file content after save>",
    "/abs/path/file2.ext": "<full file content after save>"
  }
}
```

Key requirements:
- **500ms debounce per repo root** — reset the timer on each save within the window; fire once
  after the window closes. "Save All" must produce one checkpoint, not N.
- **Absolute file paths only** in both `edited_filepaths` and `dirty_files` keys.
- **Skip IDE-internal paths** (`.idea/`, `.vscode/`, `.git/`, etc.).
- **Find repo root** by walking up from the saved file path looking for a `.git` directory.
- **Async / non-blocking** — the save event handler must not block the UI thread.

Reference implementations: `agent-support/vscode/src/known-human-checkpoint-manager.ts` (TypeScript)
and `agent-support/intellij/src/main/kotlin/.../listener/DocumentSaveListener.kt` (Kotlin).

---

## Two-Layer Architecture

Each integration has two layers:

**Layer 1 — Extension/plugin code** (`agent-support/<ide>/`)  
Native code for the IDE that listens to the save event, debounces, and calls autter.

**Layer 2 — Rust installer** (`src/mdm/agents/<ide>.rs`)  
Implements the `HookInstaller` trait. Auto-installs the extension where feasible; otherwise
returns a message with manual steps. Wired into `src/mdm/agents/mod.rs`.

---

## Official Starter Templates

Each subagent **must** find and scaffold from the official starter/template for the target IDE
before writing any logic. Do not invent boilerplate from scratch. The starters to use:

| IDE | Official starter |
|-----|-----------------|
| Zed | `zed-extension-api` crate template on GitHub; or `zed extension new` CLI if available |
| Sublime Text | Package Control "Package Example" repo |
| Eclipse | Eclipse PDE Plug-in Project archetype (`mvn archetype:generate`) |
| Xcode | Xcode built-in "Source Editor Extension" template (Swift Package scaffold) |
| Vim | community `vim-plugin-template` on GitHub (search for a well-starred one) |
| Neovim | `nvim-lua/nvim-plugin-template` on GitHub |
| Visual Studio | `dotnet new vsix` / Visual Studio "VSIX Project" wizard template |
| Notepad++ | `kbilsted/NotepadPlusPlusPluginPack.Net` — canonical .NET plugin starter |

---

## Per-IDE Specifications

### PR 1 — Windsurf

**Auto-install:** Yes (via `windsurf --install-extension` CLI).  
**New extension code:** None — reuses the existing `autter.autter-vscode` VS Code extension.
The VS Code extension already detects Windsurf via `host-kind.ts`.

**`src/mdm/utils.rs`**  
Add Windsurf to `get_editor_cli_candidates()`:
- macOS: `/Applications/Windsurf.app` and `~/Applications/Windsurf.app`
- Linux: `/opt/Windsurf/`, `~/.local/share/windsurf/`, `~/.local/share/Windsurf/`
- Windows: `%LOCALAPPDATA%\Programs\Windsurf\`

**`src/mdm/agents/windsurf.rs`** (modify existing file)  
- `check_hooks()`: additionally check whether the VS Code extension is installed using
  `resolve_editor_cli("windsurf")` + `is_vsc_editor_extension_installed(&cli, "autter.autter-vscode")`.
- `install_extras()`: call `resolve_editor_cli("windsurf")` then
  `install_vsc_editor_extension(&cli, "autter.autter-vscode")`, mirroring the pattern in
  `cursor.rs` exactly. Include Codespaces guard (same as VS Code installer).
- `uninstall_extras()`: note that the extension must be uninstalled manually.

Check whether Windsurf uses the same settings directory layout as Cursor
(`~/.codeium/windsurf/` exists already for Cascade hooks). If Windsurf also respects a
`settings.json` for `git.path`, add a `settings_paths_for_products(&["Windsurf"])` call
in `install_extras()`.

---

### PR 2 — Zed

**Auto-install:** Yes (drop compiled extension into `~/.config/zed/extensions/autter/`).  
**Extension language:** Rust, compiled to WebAssembly.

**`agent-support/zed/`**  
Scaffold from the `zed-extension-api` template. Implement the `Extension` trait.

For the save hook: Zed's extension API as of early 2025 is evolving. The subagent must
verify whether `workspace_did_save` or equivalent is exposed in the current
`zed_extension_api` crate. If a stable on-save hook exists, use it directly. If not,
investigate whether a Zed "task" (`.zed/tasks.json`) can be written that runs on save and
calls autter — accepting that this requires the user to have the task configured. Document
the chosen approach clearly in the PR.

The extension must:
- Find the git repo root from the saved file path
- Debounce 500ms per repo root
- Construct the JSON payload and call autter via OS process

**`src/mdm/agents/zed.rs`**  
- `check_hooks()`: detect `zed` binary or `~/.config/zed/` directory.
- `install_extras()`: write the compiled WASM artifact + `extension.toml` into
  `~/.config/zed/extensions/autter/`. Zed discovers extensions in that directory on restart.
  The WASM bytes must be available at install time — embed them in the Rust binary via
  `include_bytes!` pointing to a pre-built artifact committed to `agent-support/zed/dist/`,
  or build them on-the-fly if `cargo` + `wasm32-wasi` toolchain is present. Document the
  chosen approach in the PR.
- `install_hooks()` / `uninstall_hooks()`: no-ops.

---

### PR 3 — Sublime Text

**Auto-install:** Yes (drop Python package into Packages directory, hot-reloaded).  
**Extension language:** Python 3.

**`agent-support/sublime-text/`**  
Scaffold from the Package Control "Package Example" repo. Single-file plugin `autter.py`.

Implement `sublime_plugin.EventListener` with `on_post_save_async(self, view)`:
- Get absolute file path from `view.file_name()`
- Skip paths containing `.git/`, `.sublime-workspace`, `.sublime-project`
- Walk up from file path to find `.git/` directory (repo root)
- Per-repo-root debounce: cancel existing `threading.Timer` for that root, start new 500ms timer
- On timer fire: read current file contents (re-read from disk, the file is already saved),
  build JSON payload, call autter via `subprocess.Popen` with JSON on stdin
- Use `sublime.packages_path()` at runtime to locate the autter binary relative to the
  known install path (`~/.autter/bin/autter`), not relative to the package

**`src/mdm/agents/sublime_text.rs`**  
Packages path by platform:
- macOS: `~/Library/Application Support/Sublime Text/Packages/`
- Linux: `~/.config/sublime-text/Packages/`
- Windows: `%APPDATA%\Sublime Text\Packages\`

- `check_hooks()`: detect `subl` binary or the Packages directory. Check whether
  `Packages/autter/autter.py` exists and contains the current binary path.
- `install_extras()`: write `autter.py` (with binary path substituted) into
  `Packages/autter/autter.py`. Sublime hot-reloads on write — no restart needed.
- `install_hooks()` / `uninstall_hooks()`: no-ops.

---

### PR 4 — Eclipse

**Auto-install:** No.  
**Extension language:** Java (OSGi bundle / Eclipse plugin).

**`agent-support/eclipse/`**  
Scaffold via Eclipse PDE `mvn archetype:generate` using the standard plug-in archetype.
Maven multi-module project: one `plugin` module and one `feature` module (for the p2
update site).

Plugin implementation:
- Register an `IResourceChangeListener` on `ResourcesPlugin.getWorkspace()` at bundle
  activation for `IResourceChangeEvent.POST_CHANGE`.
- Walk `IResourceDelta` tree; collect files with `CHANGED | CONTENT` flag that are not in
  `.git/` or `.settings/` directories.
- Per-repo-root debounce: `ScheduledExecutorService` with `ScheduledFuture` cancel/reschedule.
- On fire: read file contents from `IFile.getContents()`, build JSON, call autter via
  `ProcessBuilder`.
- Find repo root: walk `IResource.getParent()` up to workspace root, checking for `.git`.
- Bundle ID: `io.autter.eclipse`.

**`src/mdm/agents/eclipse.rs`**  
- `check_hooks()`: detect `eclipse` binary or `~/.p2/` (Eclipse provisioning metadata).
- `install_extras()`: no auto-install. Return a message:
  ```
  Eclipse: Install from the update site — Help → Install New Software →
  Add site: <update-site-url-tbd> — then restart Eclipse.
  ```
  (The update site URL is TBD; the subagent should use a placeholder and note it in the PR.)
- PR description must include full manual install steps for the docs site.

---

### PR 5 — Xcode

**Auto-install:** No.  
**Approach:** Lightweight Swift CLI daemon using `FSEvents`, not a Source Editor Extension
(Xcode extensions don't receive save events).

**`agent-support/xcode/`**  
Scaffold from Xcode's Swift Package template. Build target: `autter-xcode-watcher` CLI.

Implementation:
- Accept `--path <dir>` argument(s) (the Xcode project root directory).
- Use `FSEvents` (`FSEventStreamCreate`) to watch for file-system events under each path.
- Filter for actual file writes (not directory events, not `.git/` or `DerivedData/`).
- 500ms debounce per repo root (same logic as other integrations).
- On fire: read saved file contents, build JSON payload, call autter.
- Can run as a persistent process (e.g., via launchd) or as a one-shot watcher.

Users integrate by adding `autter-xcode-watcher --path "$SRCROOT"` to their Xcode scheme's
"Pre-action" or "Post-action" run scripts, or by installing the provided launchd plist.

**`src/mdm/agents/xcode.rs`**  
- `check_hooks()`: macOS only; detect `/Applications/Xcode.app`.
- `install_extras()`: no auto-install. Return instructions:
  ```
  Xcode: Run 'autter-xcode-watcher --path <project-dir>' as a background process,
  or add it to your Xcode scheme's Pre-action script. See docs for launchd setup.
  ```
- PR description must include scheme script snippet and launchd plist for the docs site.

---

### PR 6 — Vim

**Auto-install:** No.  
**Extension language:** Vimscript.

**`agent-support/vim/`**  
Scaffold from a well-starred community `vim-plugin-template` on GitHub (subagent to find
the best current option). Structure: `plugin/autter.vim` (auto-loaded), `doc/autter.txt`.

`plugin/autter.vim` implementation:
- Guard: `if exists('g:loaded_autter') | finish | endif`
- `autocmd BufWritePost * call s:AutterKnownHuman(expand('<afile>:p'))`
- Per-repo-root debounce: `timer_stop()` on existing timer for that root, `timer_start(500, ...)`
- On timer fire: construct JSON string, call autter via `job_start(['autter', 'checkpoint',
  'known_human', '--hook-input', 'stdin'], {'in_io': 'pipe'})` with JSON piped to `in_buf`.
  Fall back to synchronous `system()` for Vim < 8.0 (no job API).
- Find repo root: `systemlist('git -C ' . shellescape(dir) . ' rev-parse --show-toplevel')[0]`
- Skip internal paths: files under `.git/`.
- Respects `g:autter_enabled` global flag (default on).

**`src/mdm/agents/vim.rs`**  
- `check_hooks()`: detect `vim` binary.
- `install_extras()`: no auto-install. Return message with all package manager variants:
  - Native: copy to `~/.vim/pack/autter/start/autter/`
  - vim-plug: `Plug 'autter-dev/autter-cli', {'rtp': 'agent-support/vim'}`
  - Vundle: `Plugin 'autter-dev/autter-cli'`
- PR description includes all install variants for the docs site.

---

### PR 7 — Neovim

**Auto-install:** No.  
**Extension language:** Lua.

**`agent-support/neovim/`**  
Scaffold from `nvim-lua/nvim-plugin-template` on GitHub. Structure:
`lua/autter/init.lua` (main module), `plugin/autter.lua` (auto-load shim).

`lua/autter/init.lua` implementation:
- `M.setup(opts)` entry point (lazy.nvim / packer convention).
- `vim.api.nvim_create_autocmd('BufWritePost', { callback = on_save })`.
- Per-repo-root debounce: `vim.loop.new_timer()` stored in module-level table keyed by repo
  root; `timer:stop()` + `timer:start(500, 0, callback)` on each save.
- On fire: read file with `io.open(path, 'r')`, construct JSON, call autter via
  `vim.loop.spawn('autter', { args = {'checkpoint', 'known_human', '--hook-input', 'stdin'},
  stdio = {stdin_pipe, nil, nil} })`, write JSON to stdin pipe.
- Find repo root: `vim.fn.systemlist({'git', '-C', dir, 'rev-parse', '--show-toplevel'})[1]`.
- Skip `.git/` internal paths.
- Respects `require('autter').setup({ enabled = false })` opt-out.

**`src/mdm/agents/neovim.rs`**  
- `check_hooks()`: detect `nvim` binary.
- `install_extras()`: no auto-install. Return message with package manager variants:
  - lazy.nvim: `{ 'autter-dev/autter-cli', opts = {} }`
  - packer: `use 'autter-dev/autter-cli'`
  - Native: copy `lua/` + `plugin/` to `~/.config/nvim/`
- PR description includes all variants for the docs site.

---

### PR 8 — Visual Studio

**Auto-install:** Yes (via `VSIXInstaller.exe /quiet`).  
**Extension language:** C# (AsyncPackage VSIX).

**`agent-support/visual-studio/`**  
Scaffold from `dotnet new vsix` or the Visual Studio "VSIX Project" wizard template.
Target: VS 2019 (16.0+) and VS 2022 (17.0+). Framework: .NET Framework 4.7.2.

Plugin implementation:
- `AutterPackage : AsyncPackage` — initialize on IDE load.
- Get `IVsRunningDocumentTable` service and register `IVsRunningDocTableEvents3`.
- `OnAfterSave(uint docCookie)`: resolve file path from the document table, skip `.git/`
  internal paths, find repo root by walking up.
- Per-repo-root debounce: `System.Threading.Timer` (cancel on new save, restart 500ms).
- On fire: `File.ReadAllText()` for each pending path, build JSON, call autter via
  `Process.Start()` with `RedirectStandardInput = true`, write JSON to stdin.
- Package targets: `<InstallationTarget Version="[16.0, 18.0)">` covering VS2019 + VS2022.

**`src/mdm/agents/visual_studio.rs`**  
- `check_hooks()`: Windows-only. Detect VS installations by scanning
  `%ProgramFiles%\Microsoft Visual Studio\` subdirectories for `devenv.exe`. Check whether
  the VSIX is already installed by inspecting the VS extension manifest directories.
- `install_extras()`: locate `VSIXInstaller.exe` within the VS installation
  (`Common7\IDE\VSIXInstaller.exe`). Spawn `VSIXInstaller.exe /quiet /admin <path-to-vsix>`
  with the VSIX path resolved from the autter binary's sibling `lib/` directory (or embedded
  in the binary as bytes, written to a temp file). Attempt for each detected VS installation.
  Fall back to a Marketplace link message if VSIXInstaller not found.
- `uninstall_extras()`: run `VSIXInstaller.exe /quiet /uninstall:io.autter.visualstudio`.
- PR description includes manual Marketplace install steps for the docs site.

---

### PR 9 — Notepad++

**Auto-install:** Yes (copy DLL to plugins directory).  
**Extension language:** C# (.NET Framework 4.8, compiled to DLL).

**`agent-support/notepad-plus-plus/`**  
Scaffold from `kbilsted/NotepadPlusPlusPluginPack.Net` (the canonical .NET plugin starter).
The template provides the `NppPlugin` base class and P/Invoke declarations.

Plugin implementation:
- Handle `NPPN_FILESAVED` notification in `beNotified(ScNotification notification)`.
- Extract the saved file path by calling `Win32.SendMessage(nppHandle,
  NppMsg.NPPM_GETFULLCURRENTPATH, ...)` — do not use `notification.nmhdr.idFrom`
  (that is the control ID, not the path).
- Skip `.git\` internal paths.
- Per-repo-root debounce: `System.Threading.Timer` (cancel/restart 500ms).
- On fire: `File.ReadAllText()`, find repo root by walking up, build JSON, call autter via
  `Process.Start()` with `RedirectStandardInput = true`.
- Build both x86 and x64 DLL variants (Notepad++ ships both architectures).
- DLL name: `autter.dll` inside folder `autter\` (Notepad++ 8.x plugin layout).

**`src/mdm/agents/notepad_plus_plus.rs`**  
- `check_hooks()`: Windows-only. Detect Notepad++ via
  `%ProgramFiles%\Notepad++\notepad++.exe` or `%ProgramFiles(x86)%\Notepad++\` or
  registry `HKCU\Software\Notepad++`. Check whether
  `%APPDATA%\Notepad++\plugins\autter\autter.dll` exists.
- `install_extras()`: determine Notepad++ architecture (read PE header of `notepad++.exe` to
  choose x86 vs x64 DLL). Copy the matching DLL to
  `%APPDATA%\Notepad++\plugins\autter\autter.dll`. Notepad++ loads plugins from this
  directory on next launch. Return a note that Notepad++ must be restarted.
- `uninstall_extras()`: delete `%APPDATA%\Notepad++\plugins\autter\`.
- PR description includes manual install steps (download DLL, place in folder) for the docs site.

---

## Subagent Execution Plan

Each PR is implemented by one subagent in its own git worktree branching from `main`.
All 9 run in parallel. Each subagent must:

1. Read this design doc and the reference implementations (`vscode/known-human-checkpoint-manager.ts`,
   `intellij/.../DocumentSaveListener.kt`, `src/mdm/agents/cursor.rs`, `src/mdm/agents/vscode.rs`).
2. Find and scaffold from the official starter template listed in the "Official Starter Templates"
   section above. Do not invent boilerplate.
3. Implement the extension/plugin code in `agent-support/<ide>/`.
4. Implement the Rust installer in `src/mdm/agents/<ide>.rs`.
5. Wire the new installer into `src/mdm/agents/mod.rs` (`get_all_installers()`).
6. For IDEs with manual-only install: write clear manual steps in the PR description,
   structured for copy-paste into the docs site.
7. Open a PR against `main`.

## Non-Goals

- Emacs integration (explicitly deferred).
- Changes to the `known_human` checkpoint protocol or backend logic.
- Automated tests for the extension/plugin code (integration-tested by the existing
  Rust test suite via the checkpoint pathway).
