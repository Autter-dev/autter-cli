# Multi-IDE Known-Human Checkpoint — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `known_human` checkpoint support (fire `autter checkpoint known_human --hook-input stdin` on file save with 500ms debounce) to 9 IDE/editor targets, one PR each, running as parallel worktree subagents.

**Architecture:** Each PR follows the two-layer pattern established by VS Code (agent-support/vscode/) and JetBrains (agent-support/intellij/): (1) extension/plugin code that hooks the IDE's save event and calls autter, and (2) a Rust installer in src/mdm/agents/ that auto-installs the extension where possible. All 9 are independent — no shared code between PRs.

**Tech Stack:** Rust (all installers), TypeScript/Python/Java/Swift/Vimscript/Lua/C# per IDE, HookInstaller trait (src/mdm/hook_installer.rs).

**Spec:** docs/superpowers/specs/2026-04-11-multi-ide-known-human-design.md

---

## Common Pattern (applies to all 9 tasks)

### Reference files to read first (every subagent):
- `docs/superpowers/specs/2026-04-11-multi-ide-known-human-design.md` — full spec
- `agent-support/vscode/src/known-human-checkpoint-manager.ts` — TypeScript reference
- `agent-support/intellij/src/main/kotlin/org/jetbrains/plugins/template/listener/DocumentSaveListener.kt` — Kotlin reference
- `src/mdm/agents/cursor.rs` — extension-based installer with auto-install pattern
- `src/mdm/agents/vscode.rs` — extension-based installer
- `src/mdm/hook_installer.rs` — HookInstaller trait
- `src/mdm/utils.rs` — binary_exists(), home_dir(), resolve_editor_cli(), install_vsc_editor_extension()
- `src/mdm/agents/mod.rs` — wiring pattern

### Rust installer skeleton (adapt for each IDE):
```rust
use crate::error::AutterError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams, InstallResult, UninstallResult};
use crate::mdm::utils::{binary_exists, home_dir};

pub struct <Name>Installer;

impl HookInstaller for <Name>Installer {
    fn name(&self) -> &str { "<Human Name>" }
    fn id(&self) -> &str { "<kebab-id>" }
    fn uses_config_hooks(&self) -> bool { false }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, AutterError> {
        let detected = binary_exists("<binary>") || home_dir().join(".<dir>").exists();
        Ok(HookCheckResult {
            tool_installed: detected,
            hooks_installed: false,  // update to check actual install state
            hooks_up_to_date: false,
        })
    }

    fn install_hooks(&self, _params: &HookInstallerParams, _dry_run: bool) -> Result<Option<String>, AutterError> {
        Ok(None)  // extension-based: no config file hooks
    }

    fn uninstall_hooks(&self, _params: &HookInstallerParams, _dry_run: bool) -> Result<Option<String>, AutterError> {
        Ok(None)
    }

    fn install_extras(&self, params: &HookInstallerParams, dry_run: bool) -> Result<Vec<InstallResult>, AutterError> {
        // auto-install logic or manual message
        todo!()
    }
}
```

### mod.rs wiring — add to BOTH sections:
```rust
// At top (mod declarations):
mod <name>;
pub use <name>::<Name>Installer;

// In get_all_installers():
Box::new(<Name>Installer),
```

### CI commands (run before PR):
```bash
cargo check 2>&1 | head -50
cargo clippy -- -D warnings 2>&1 | head -50
cargo test -- --test-threads=4 2>&1 | tail -20
```

### PR creation:
```bash
git push -u origin <branch-name>
gh pr create --title "<title>" --body "$(cat <<'EOF'
## Summary
- <bullet points>

## Manual install steps (for docs site)
<steps if applicable>

## Test plan
- [ ] cargo clippy passes
- [ ] cargo test passes

🤖 Generated with Claude Code
EOF
)"
```

---

## Task 1 — Windsurf

**Branch:** `feat/known-human-windsurf`  
**Files modified** (no new extension code):
- `src/mdm/utils.rs` — add windsurf to `get_editor_cli_candidates()`
- `src/mdm/agents/windsurf.rs` — add `install_extras()` + update `check_hooks()`

- [ ] Read reference files listed in Common Pattern above
- [ ] In `src/mdm/utils.rs`, find the `get_editor_cli_candidates()` match arm for `"cursor"` and add a `"windsurf"` arm after it:
```rust
"windsurf" => {
    #[cfg(target_os = "macos")]
    {
        for apps_dir in [PathBuf::from("/Applications"), home.join("Applications")] {
            let app = apps_dir.join("Windsurf.app");
            candidates.push((
                app.join("Contents").join("MacOS").join("Windsurf"),
                app.join("Contents").join("Resources").join("app").join("out").join("cli.js"),
            ));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for base in [
            PathBuf::from("/opt/Windsurf"),
            home.join(".local").join("share").join("windsurf"),
            home.join(".local").join("share").join("Windsurf"),
        ] {
            candidates.push((
                base.join("windsurf"),
                base.join("resources").join("app").join("out").join("cli.js"),
            ));
        }
    }
    #[cfg(windows)]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            let base = PathBuf::from(local_app_data).join("Programs").join("Windsurf");
            candidates.push((
                base.join("Windsurf.exe"),
                base.join("resources").join("app").join("out").join("cli.js"),
            ));
        }
    }
}
```
- [ ] In `src/mdm/agents/windsurf.rs`, add `install_vsc_editor_extension`, `is_vsc_editor_extension_installed`, `resolve_editor_cli` to imports, then add `install_extras()` method mirroring `cursor.rs` exactly (substitute `"windsurf"` for `"cursor"`, extension ID stays `"autter.autter-vscode"`). Also update `check_hooks()` to check extension install status using `resolve_editor_cli("windsurf")`.
- [ ] `cargo check && cargo clippy -- -D warnings`
- [ ] `cargo test 2>&1 | tail -20`
- [ ] Commit: `git commit -m "feat(windsurf): auto-install VS Code extension for known_human checkpoint"`
- [ ] Push and create PR

---

## Task 2 — Zed

**Branch:** `feat/known-human-zed`  
**New files:**
- `agent-support/zed/Cargo.toml`
- `agent-support/zed/src/lib.rs`
- `agent-support/zed/extension.toml`
- `src/mdm/agents/zed.rs`

- [ ] Read reference files listed in Common Pattern above
- [ ] Research the current `zed_extension_api` crate on crates.io/GitHub to understand the 2025 API. Check if `on_save` or `workspace_did_save` hooks exist. If not, document the fallback approach chosen.
- [ ] Scaffold from `zed-extension-api` template. Create `agent-support/zed/` with proper Cargo.toml targeting `wasm32-wasip1`.
- [ ] Implement the save hook in `agent-support/zed/src/lib.rs`:
  - Listen for file-save events
  - 500ms debounce per repo root
  - Build JSON payload with `editor: "zed"`, `editor_version`, `cwd`, `edited_filepaths`, `dirty_files`
  - Spawn `autter checkpoint known_human --hook-input stdin` with JSON on stdin
- [ ] Create `src/mdm/agents/zed.rs` — detect `zed` binary or `~/.config/zed/`. `install_extras()` writes the WASM + extension.toml to `~/.config/zed/extensions/autter/` (embed WASM via `include_bytes!` from pre-built `agent-support/zed/dist/extension.wasm` if possible, or document build step in PR).
- [ ] Wire into `src/mdm/agents/mod.rs`
- [ ] `cargo check && cargo clippy -- -D warnings && cargo test 2>&1 | tail -20`
- [ ] Commit and create PR

---

## Task 3 — Sublime Text

**Branch:** `feat/known-human-sublime-text`  
**New files:**
- `agent-support/sublime-text/autter.py`
- `src/mdm/agents/sublime_text.rs`

- [ ] Read reference files listed in Common Pattern above
- [ ] Find the Package Control "Package Example" repo for boilerplate structure reference
- [ ] Create `agent-support/sublime-text/autter.py`:
```python
import sublime
import sublime_plugin
import subprocess
import threading
import json
import os

_timers = {}  # repo_root -> threading.Timer
_pending = {}  # repo_root -> list of (path, content)
_lock = threading.Lock()

AUTTER_BIN = os.path.expanduser("~/.autter/bin/autter")

def find_repo_root(path):
    d = os.path.dirname(path)
    while d != os.path.dirname(d):
        if os.path.isdir(os.path.join(d, ".git")):
            return d
        d = os.path.dirname(d)
    return None

def fire_checkpoint(repo_root):
    with _lock:
        files = list(_pending.get(repo_root, []))
        _pending[repo_root] = []
    if not files:
        return
    dirty_files = {}
    for path, content in files:
        dirty_files[path] = content
    payload = json.dumps({
        "editor": "sublime-text",
        "editor_version": sublime.version(),
        "extension_version": "1.0.0",
        "cwd": repo_root,
        "edited_filepaths": list(dirty_files.keys()),
        "dirty_files": dirty_files,
    })
    try:
        proc = subprocess.Popen(
            [AUTTER_BIN, "checkpoint", "known_human", "--hook-input", "stdin"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            cwd=repo_root,
        )
        proc.stdin.write(payload.encode("utf-8"))
        proc.stdin.close()
        proc.wait(timeout=10)
    except Exception as e:
        print(f"[autter] checkpoint error: {e}")

class AutterSaveListener(sublime_plugin.EventListener):
    def on_post_save_async(self, view):
        path = view.file_name()
        if not path:
            return
        if "/.git/" in path.replace("\\", "/"):
            return
        if path.endswith(".sublime-workspace") or path.endswith(".sublime-project"):
            return
        repo_root = find_repo_root(path)
        if not repo_root:
            return
        content = view.substr(sublime.Region(0, view.size()))
        with _lock:
            if repo_root not in _pending:
                _pending[repo_root] = []
            # Update or append entry for this path
            existing = [(p, c) for p, c in _pending[repo_root] if p != path]
            existing.append((path, content))
            _pending[repo_root] = existing
            if repo_root in _timers:
                _timers[repo_root].cancel()
            t = threading.Timer(0.5, fire_checkpoint, args=[repo_root])
            _timers[repo_root] = t
            t.start()
```
- [ ] Create `src/mdm/agents/sublime_text.rs` with `SublimeTextInstaller`. `check_hooks()` detects `subl` binary or Packages directory. `install_extras()` writes `autter.py` (with AUTTER_BIN path substituted to `params.binary_path`) to `Packages/autter/autter.py`. Platform paths: macOS `~/Library/Application Support/Sublime Text/Packages/`, Linux `~/.config/sublime-text/Packages/`, Windows `%APPDATA%\Sublime Text\Packages\`.
- [ ] Wire into mod.rs
- [ ] `cargo check && cargo clippy -- -D warnings && cargo test 2>&1 | tail -20`
- [ ] Commit and create PR

---

## Task 4 — Eclipse

**Branch:** `feat/known-human-eclipse`  
**New files:**
- `agent-support/eclipse/` (Maven multi-module: plugin + feature)
- `src/mdm/agents/eclipse.rs`

- [ ] Read reference files listed in Common Pattern above
- [ ] Scaffold via Eclipse PDE Maven archetype or manually create the OSGi bundle structure. Key files: `MANIFEST.MF`, `plugin.xml`, `pom.xml`, main `Activator.java`, `AutterSaveListener.java`.
- [ ] Implement `AutterSaveListener implements IResourceChangeListener` in `agent-support/eclipse/plugin/src/io/autter/eclipse/`:
  - Register on `ResourcesPlugin.getWorkspace().addResourceChangeListener(this, IResourceChangeEvent.POST_CHANGE)`
  - Walk delta, collect `IResourceDelta.CHANGED | IResourceDelta.CONTENT` files not under `.git/`
  - 500ms debounce per repo root using `ScheduledExecutorService`
  - On fire: read contents via `IFile.getContents()`, build JSON, call autter via `ProcessBuilder`
  - Find repo root: walk `IContainer.getParent()` checking for `.git` child
- [ ] Create `src/mdm/agents/eclipse.rs` — `check_hooks()` detects `eclipse` binary or `~/.p2/`. `install_extras()` returns manual message with placeholder update site URL.
- [ ] Wire into mod.rs
- [ ] `cargo check && cargo clippy -- -D warnings && cargo test 2>&1 | tail -20`
- [ ] Commit and create PR (include full manual install steps in PR body)

---

## Task 5 — Xcode

**Branch:** `feat/known-human-xcode`  
**New files:**
- `agent-support/xcode/` (Swift Package, `autter-xcode-watcher` CLI target)
- `src/mdm/agents/xcode.rs`

- [ ] Read reference files listed in Common Pattern above
- [ ] Scaffold a Swift Package with `swift package init --type executable`. Rename the target to `autter-xcode-watcher`.
- [ ] Implement `Sources/autter-xcode-watcher/main.swift`:
  - Parse `--path <dir>` args (multiple allowed)
  - Use `CoreServices.FSEventStreamCreate` to watch each directory
  - Filter events: skip `.git/`, `DerivedData/`, `xcuserdata/`, `.build/`
  - 500ms debounce per git repo root (find root with `git -C <dir> rev-parse --show-toplevel`)
  - On fire: read file contents, build JSON payload, exec `autter checkpoint known_human --hook-input stdin`
- [ ] Create `src/mdm/agents/xcode.rs` — macOS only (`#[cfg(target_os = "macos")]`). `check_hooks()` detects `/Applications/Xcode.app`. `install_extras()` returns instructions for adding to Xcode scheme pre-action and launchd plist.
- [ ] Wire into mod.rs (wrap with `#[cfg(target_os = "macos")]` if needed, or use runtime detection)
- [ ] `cargo check && cargo clippy -- -D warnings && cargo test 2>&1 | tail -20`
- [ ] Commit and create PR (include scheme script + launchd plist in PR body)

---

## Task 6 — Vim

**Branch:** `feat/known-human-vim`  
**New files:**
- `agent-support/vim/plugin/autter.vim`
- `agent-support/vim/doc/autter.txt`
- `src/mdm/agents/vim.rs`

- [ ] Read reference files listed in Common Pattern above
- [ ] Find a well-starred `vim-plugin-template` on GitHub for structural reference
- [ ] Create `agent-support/vim/plugin/autter.vim`:
```vim
if exists('g:loaded_autter') | finish | endif
let g:loaded_autter = 1

if !exists('g:autter_enabled')
  let g:autter_enabled = 1
endif

let s:debounce_timers = {}
let s:pending_files = {}

function! s:AutterBin() abort
  return expand('~/.autter/bin/autter')
endfunction

function! s:FindRepoRoot(file) abort
  let l:dir = fnamemodify(a:file, ':h')
  let l:result = systemlist('git -C ' . shellescape(l:dir) . ' rev-parse --show-toplevel 2>/dev/null')
  return empty(l:result) ? '' : l:result[0]
endfunction

function! s:FireCheckpoint(root) abort
  let l:files = get(s:pending_files, a:root, {})
  if empty(l:files) | return | endif
  let s:pending_files[a:root] = {}

  let l:dirty = {}
  for [l:path, l:content] in items(l:files)
    let l:dirty[l:path] = l:content
  endfor

  let l:payload = json_encode({
    \ 'editor': 'vim',
    \ 'editor_version': string(v:version),
    \ 'extension_version': '1.0.0',
    \ 'cwd': a:root,
    \ 'edited_filepaths': keys(l:dirty),
    \ 'dirty_files': l:dirty,
    \ })

  let l:cmd = [s:AutterBin(), 'checkpoint', 'known_human', '--hook-input', 'stdin']
  if has('job')
    let l:job = job_start(l:cmd, {
      \ 'in_mode': 'raw',
      \ 'cwd': a:root,
      \ })
    call ch_sendraw(job_getchannel(l:job), l:payload)
    call ch_close_in(job_getchannel(l:job))
  else
    " Fallback: synchronous (blocks briefly on slow systems)
    call system(join(map(copy(l:cmd), 'shellescape(v:val)'), ' '), l:payload)
  endif
endfunction

function! s:OnSave() abort
  if !g:autter_enabled | return | endif
  let l:file = expand('<afile>:p')
  if l:file =~# '/\.git/' | return | endif
  let l:root = s:FindRepoRoot(l:file)
  if empty(l:root) | return | endif
  let l:content = join(getline(1, '$'), "\n")
  if !has_key(s:pending_files, l:root)
    let s:pending_files[l:root] = {}
  endif
  let s:pending_files[l:root][l:file] = l:content
  if has_key(s:debounce_timers, l:root)
    call timer_stop(s:debounce_timers[l:root])
  endif
  let s:debounce_timers[l:root] = timer_start(500, {-> s:FireCheckpoint(l:root)})
endfunction

augroup autter_known_human
  autocmd!
  autocmd BufWritePost * call s:OnSave()
augroup END
```
- [ ] Create minimal `agent-support/vim/doc/autter.txt` help file
- [ ] Create `src/mdm/agents/vim.rs` — `check_hooks()` detects `vim` binary. `install_extras()` returns message listing all install variants (native pack, vim-plug, Vundle).
- [ ] Wire into mod.rs
- [ ] `cargo check && cargo clippy -- -D warnings && cargo test 2>&1 | tail -20`
- [ ] Commit and create PR (include all package manager install variants in PR body)

---

## Task 7 — Neovim

**Branch:** `feat/known-human-neovim`  
**New files:**
- `agent-support/neovim/lua/autter/init.lua`
- `agent-support/neovim/plugin/autter.lua`
- `agent-support/neovim/README.md`
- `src/mdm/agents/neovim.rs`

- [ ] Read reference files listed in Common Pattern above
- [ ] Check out `nvim-lua/nvim-plugin-template` on GitHub for structural reference
- [ ] Create `agent-support/neovim/plugin/autter.lua` (auto-load shim):
```lua
-- Auto-load shim: loaded by Neovim on startup
if vim.g.loaded_autter then return end
vim.g.loaded_autter = 1
require('autter').setup()
```
- [ ] Create `agent-support/neovim/lua/autter/init.lua`:
```lua
local M = {}
local timers = {}   -- repo_root -> uv timer
local pending = {}  -- repo_root -> { [path] = content }

local function autter_bin()
  return vim.fn.expand('~/.autter/bin/autter')
end

local function find_repo_root(file)
  local dir = vim.fn.fnamemodify(file, ':h')
  local result = vim.fn.systemlist({'git', '-C', dir, 'rev-parse', '--show-toplevel'})
  if vim.v.shell_error ~= 0 or #result == 0 then return nil end
  return result[1]
end

local function fire(root)
  local files = pending[root]
  if not files or next(files) == nil then return end
  pending[root] = {}

  local dirty = {}
  local paths = {}
  for path, content in pairs(files) do
    dirty[path] = content
    table.insert(paths, path)
  end

  local payload = vim.json.encode({
    editor = 'neovim',
    editor_version = tostring(vim.version()),
    extension_version = '1.0.0',
    cwd = root,
    edited_filepaths = paths,
    dirty_files = dirty,
  })

  local stdin = vim.loop.new_pipe(false)
  vim.loop.spawn(autter_bin(), {
    args = {'checkpoint', 'known_human', '--hook-input', 'stdin'},
    stdio = {stdin, nil, nil},
    cwd = root,
  }, function() end)
  vim.loop.write(stdin, payload)
  vim.loop.shutdown(stdin, function() vim.loop.close(stdin) end)
end

local function on_save(args)
  if not vim.g.autter_enabled then return end
  local file = vim.api.nvim_buf_get_name(args.buf)
  if file == '' then return end
  if file:find('/.git/') then return end
  local root = find_repo_root(file)
  if not root then return end

  local lines = vim.api.nvim_buf_get_lines(args.buf, 0, -1, false)
  local content = table.concat(lines, '\n')

  if not pending[root] then pending[root] = {} end
  pending[root][file] = content

  if timers[root] then
    timers[root]:stop()
  else
    timers[root] = vim.loop.new_timer()
  end
  timers[root]:start(500, 0, vim.schedule_wrap(function() fire(root) end))
end

function M.setup(opts)
  opts = opts or {}
  vim.g.autter_enabled = opts.enabled ~= false
  vim.api.nvim_create_autocmd('BufWritePost', {
    group = vim.api.nvim_create_augroup('AutterKnownHuman', { clear = true }),
    callback = on_save,
  })
end

return M
```
- [ ] Create `src/mdm/agents/neovim.rs` — `check_hooks()` detects `nvim` binary. `install_extras()` returns message with lazy.nvim, packer, and native install variants.
- [ ] Wire into mod.rs
- [ ] `cargo check && cargo clippy -- -D warnings && cargo test 2>&1 | tail -20`
- [ ] Commit and create PR (include all package manager install variants in PR body)

---

## Task 8 — Visual Studio

**Branch:** `feat/known-human-visual-studio`  
**New files:**
- `agent-support/visual-studio/` (C# VSIX project)
- `src/mdm/agents/visual_studio.rs`

- [ ] Read reference files listed in Common Pattern above
- [ ] Research `dotnet new vsix` template or Visual Studio VSIX Project wizard. Scaffold the project structure.
- [ ] Implement C# VSIX in `agent-support/visual-studio/`:
  - `AutterPackage.cs`: `AsyncPackage` subclass, `InitializeAsync` gets `IVsRunningDocumentTable` and subscribes
  - `DocumentSaveListener.cs`: implements `IVsRunningDocTableEvents3.OnAfterSave`, 500ms `System.Threading.Timer` debounce per solution root, `Process.Start` to call autter with JSON on stdin
  - `.vsixmanifest`: target VS 2019 (16.0) + VS 2022 (17.0)
- [ ] Create `src/mdm/agents/visual_studio.rs` — Windows-only. `check_hooks()` scans `%ProgramFiles%\Microsoft Visual Studio\` for `devenv.exe`. `install_extras()` finds `Common7\IDE\VSIXInstaller.exe` and runs it quietly with the bundled VSIX (or returns Marketplace fallback message). `uninstall_extras()` runs `/uninstall:io.autter.visualstudio`.
- [ ] Wire into mod.rs
- [ ] `cargo check && cargo clippy -- -D warnings && cargo test 2>&1 | tail -20`
- [ ] Commit and create PR (include Marketplace manual install steps in PR body)

---

## Task 9 — Notepad++

**Branch:** `feat/known-human-notepad-plus-plus`  
**New files:**
- `agent-support/notepad-plus-plus/` (C# .NET 4.8 class library)
- `src/mdm/agents/notepad_plus_plus.rs`

- [ ] Read reference files listed in Common Pattern above
- [ ] Scaffold from `kbilsted/NotepadPlusPlusPluginPack.Net` template on GitHub
- [ ] Implement C# plugin in `agent-support/notepad-plus-plus/`:
  - `Plugin.cs`: hook `NPPN_FILESAVED` in `beNotified()`, get path via `NPPM_GETFULLCURRENTPATH`, 500ms `System.Threading.Timer` debounce per repo root, `Process.Start` with JSON on stdin
  - Build both x86 and x64 DLL variants
  - DLL output name: `autter.dll`
- [ ] Create `src/mdm/agents/notepad_plus_plus.rs` — Windows-only. `check_hooks()` detects `%ProgramFiles%\Notepad++\notepad++.exe` or registry key. `install_extras()` reads PE header of notepad++.exe to choose x86/x64 DLL, copies to `%APPDATA%\Notepad++\plugins\autter\autter.dll`. Returns note that Notepad++ must be restarted.
- [ ] Wire into mod.rs
- [ ] `cargo check && cargo clippy -- -D warnings && cargo test 2>&1 | tail -20`
- [ ] Commit and create PR (include manual DLL copy steps in PR body)
