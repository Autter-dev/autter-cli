//! `autter memory-hook <agent> [--event <name>] --hook-input stdin`: give
//! coding agents the repository's memory before they write code.
//!
//! Installed next to the checkpoint hooks for agents whose hooks can add text
//! to the model's context. On each hook it asks Autter for a briefing
//! (`GET /api/cli/orgs/<org>/repo-memory/brief`) and prints it in the agent's
//! hook output format:
//!
//! - session start: the repository memory map, what has not worked, and the
//!   list of area pages (once per session);
//! - prompt submitted: the memory sections that match the request (and the
//!   session briefing first, for agents without a session-start hook);
//! - a file read or edited: the area page covering it, once per area per
//!   session, so the agent has it before (or right as) it changes that code.
//!
//! The hook must never get in the agent's way: every failure (not logged in,
//! no network, a repository Autter does not know, a slow answer) prints
//! nothing and exits 0. It is off when the `memory_context` feature flag is
//! off (`autter config set feature_flags.memory_context false`, or
//! `AUTTER_MEMORY_CONTEXT=false`), and in `local` prompt storage mode the
//! prompt text is never sent (the session and file briefings still work; they
//! send only the repository and file paths).

use crate::api::client::ApiContext;
use crate::auth::identity::extract_identity_from_access_token;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Seconds the whole briefing call may take before the hook gives up.
const BRIEF_TIMEOUT_SECS: u64 = 3;
/// Session state files older than this are removed.
const STATE_MAX_AGE_SECS: u64 = 3 * 24 * 3600;
/// Claude Code caps hook context at 10,000 characters (Codex at about 2,500
/// tokens by default); stay under both.
const MAX_CONTEXT_CHARS: usize = 9_500;

/// When a hook fires, from the agent's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookMoment {
    SessionStart,
    Prompt,
    File,
}

/// How an agent reads context from a hook's stdout (researched 2026-10-25;
/// sources in changelog/memory-before-code.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDialect {
    /// `{"hookSpecificOutput": {"hookEventName": <event>, "additionalContext": <text>}}`:
    /// Claude Code (SessionStart, UserPromptSubmit, PostToolUse) and Codex
    /// (the same, plus PreToolUse).
    ClaudeJson,
    /// `{"hookSpecificOutput": {"additionalContext": <text>}}`: Gemini CLI
    /// (SessionStart, BeforeAgent, AfterTool). Its docs show no event name.
    GeminiJson,
    /// Droid: plain stdout is context on SessionStart and UserPromptSubmit;
    /// PostToolUse takes the Claude shape.
    Droid,
    /// `{"additionalContext": <text>}` at the top level: GitHub Copilot CLI
    /// (sessionStart, postToolUse). Its prompt hook output is dropped.
    Copilot,
    /// `{"additional_context": <text>}`: Cursor (sessionStart, postToolUse).
    /// Documented; Cursor currently drops it (forum reports, 2026), so this
    /// starts working when Cursor fixes it.
    Cursor,
    /// The briefing as plain text, for agent plugins that inject it themselves
    /// (OpenCode, Amp, Pi).
    Plain,
}

/// Which dialect an agent's hooks speak; None for agents this command does not serve.
pub fn dialect_for(agent: &str) -> Option<HookDialect> {
    match agent {
        "claude" | "codex" => Some(HookDialect::ClaudeJson),
        "gemini" => Some(HookDialect::GeminiJson),
        "droid" => Some(HookDialect::Droid),
        "github-copilot" | "copilot" => Some(HookDialect::Copilot),
        "cursor" => Some(HookDialect::Cursor),
        "opencode" | "amp" | "pi" => Some(HookDialect::Plain),
        _ => None,
    }
}

/// What one hook invocation is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCall {
    pub moment: HookMoment,
    /// The event name, echoed back where the agent's output names it.
    pub event: String,
    pub session_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub prompt: Option<String>,
    /// Files the tool call reads or edits (absolute or relative to `cwd`).
    pub files: Vec<String>,
}

fn str_field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .filter(|s| !s.trim().is_empty())
}

/// Tools whose files are worth an area briefing: reads and edits.
fn is_file_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "read",
        "edit",
        "write",
        "multiedit",
        "notebookedit",
        "read_file",
        "read_many_files",
        "write_file",
        "replace",
        "edit_file",
        "apply_patch",
        "applypatch",
        "str_replace",
        "str_replace_editor",
        "create",
        "view",
    ]
    .iter()
    .any(|t| n == *t || n.ends_with(&format!("__{t}")))
}

/// Files named by an `apply_patch` patch (Codex).
fn patch_files(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
        })
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn tool_files(tool_input: Option<&Value>) -> Vec<String> {
    let Some(input) = tool_input else {
        return Vec::new();
    };
    // Copilot sends its arguments as a JSON string; Codex may send a bare patch.
    let parsed: Value;
    let input = match input.as_str() {
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(v) => {
                parsed = v;
                &parsed
            }
            Err(_) => return patch_files(text),
        },
        None => input,
    };
    let mut files = Vec::new();
    for key in [
        "file_path",
        "path",
        "target_file",
        "filePath",
        "absolute_path",
        "notebook_path",
    ] {
        if let Some(p) = input.get(key).and_then(|v| v.as_str())
            && !p.trim().is_empty()
        {
            files.push(p.to_string());
        }
    }
    for key in ["input", "patch"] {
        if let Some(text) = input.get(key).and_then(|v| v.as_str()) {
            files.extend(patch_files(text));
        }
    }
    if let Some(command) = input.get("command").and_then(|v| v.as_array()) {
        for part in command.iter().filter_map(|v| v.as_str()) {
            files.extend(patch_files(part));
        }
    }
    if let Some(paths) = input.get("paths").and_then(|v| v.as_array()) {
        files.extend(paths.iter().filter_map(|p| p.as_str()).map(str::to_string));
    }
    if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
        for e in edits {
            if let Some(p) = e.get("file_path").and_then(|v| v.as_str()) {
                files.push(p.to_string());
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// Read one hook payload. `event` overrides the payload's event name (agents
/// whose payload carries none get it from the installed command line). None
/// when the hook is not one this command serves.
pub fn parse_hook_call(input: &str, event: Option<&str>) -> Option<HookCall> {
    let v: Value = serde_json::from_str(input).ok()?;
    let event = event.map(str::to_string).or_else(|| {
        str_field(&v, &["hook_event_name", "hookEventName", "event"]).map(str::to_string)
    })?;
    let moment = match event.as_str() {
        "SessionStart" | "sessionStart" | "session" => HookMoment::SessionStart,
        "UserPromptSubmit"
        | "BeforeAgent"
        | "userPromptSubmitted"
        | "beforeSubmitPrompt"
        | "prompt" => HookMoment::Prompt,
        "PostToolUse" | "PreToolUse" | "AfterTool" | "BeforeTool" | "postToolUse"
        | "preToolUse" | "file" => {
            let tool = str_field(&v, &["tool_name", "toolName", "tool"]).unwrap_or("");
            if !is_file_tool(tool) {
                return None;
            }
            HookMoment::File
        }
        _ => return None,
    };
    let cwd = str_field(&v, &["cwd", "workingDirectory"])
        .map(PathBuf::from)
        .or_else(|| {
            v.get("workspace_roots")
                .or_else(|| v.get("workspacePaths"))
                .and_then(|r| r.as_array())
                .and_then(|r| r.first())
                .and_then(|r| r.as_str())
                .map(PathBuf::from)
        });
    let files = if moment == HookMoment::File {
        tool_files(
            v.get("tool_input")
                .or_else(|| v.get("toolInput"))
                .or_else(|| v.get("toolArgs"))
                .or_else(|| v.get("args")),
        )
    } else {
        Vec::new()
    };
    if moment == HookMoment::File && files.is_empty() {
        return None;
    }
    Some(HookCall {
        moment,
        event,
        session_id: str_field(&v, &["session_id", "sessionId", "conversation_id"])
            .map(str::to_string),
        cwd,
        prompt: str_field(&v, &["prompt", "user_prompt", "userPrompt"]).map(str::to_string),
        files,
    })
}

/// The hook's stdout for this context; None when there is nothing to inject.
pub fn render_output(
    dialect: HookDialect,
    moment: HookMoment,
    event: &str,
    context: &str,
) -> Option<String> {
    if context.trim().is_empty() {
        return None;
    }
    let claude = || {
        json!({
            "hookSpecificOutput": {
                "hookEventName": event,
                "additionalContext": context,
            }
        })
        .to_string()
    };
    Some(match dialect {
        HookDialect::ClaudeJson => claude(),
        HookDialect::GeminiJson => {
            json!({ "hookSpecificOutput": { "additionalContext": context } }).to_string()
        }
        HookDialect::Droid if moment == HookMoment::File => claude(),
        HookDialect::Droid | HookDialect::Plain => context.to_string(),
        HookDialect::Copilot => {
            if moment == HookMoment::Prompt {
                // Copilot drops prompt-hook output.
                return None;
            }
            // One hooks file serves the Copilot CLI (top-level field) and VS Code
            // (Claude-style field); each reads its own.
            json!({
                "additionalContext": context,
                "hookSpecificOutput": { "hookEventName": event, "additionalContext": context },
            })
            .to_string()
        }
        HookDialect::Cursor => {
            if moment == HookMoment::Prompt {
                return None;
            }
            json!({ "additional_context": context }).to_string()
        }
    })
}

// ── Per-session bookkeeping ─────────────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionState {
    /// The session briefing was injected.
    #[serde(default)]
    pub session_done: bool,
    /// Area pages (`areas/<slug>`) already injected.
    #[serde(default)]
    pub areas: Vec<String>,
    /// Every area page and the path it covers, from the session briefing.
    /// Known: a file read outside a new area needs no call at all.
    #[serde(default)]
    pub area_paths: Option<Vec<AreaPath>>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AreaPath {
    pub slug: String,
    pub path: String,
}

/// The area pages covering these repo-relative files that are not injected yet.
pub fn new_areas_for(
    paths: &[String],
    area_paths: &[AreaPath],
    injected: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for file in paths {
        let best = area_paths
            .iter()
            .filter(|a| {
                let dir = a.path.trim_start_matches("./").trim_end_matches('/');
                !dir.is_empty() && file.starts_with(&format!("{dir}/"))
            })
            .max_by_key(|a| a.path.len());
        if let Some(area) = best
            && !injected.contains(&area.slug)
            && !out.contains(&area.slug)
        {
            out.push(area.slug.clone());
        }
    }
    out
}

fn area_paths_of(brief: &Option<Value>) -> Option<Vec<AreaPath>> {
    let list = brief.as_ref()?.get("areaPaths")?.as_array()?;
    Some(
        list.iter()
            .filter_map(|a| {
                Some(AreaPath {
                    slug: a.get("slug")?.as_str()?.to_string(),
                    path: a.get("path")?.as_str()?.to_string(),
                })
            })
            .collect(),
    )
}

fn state_dir() -> Option<PathBuf> {
    crate::config::autter_dir_path().map(|d| d.join("memory-sessions"))
}

fn state_path(dir: &Path, agent: &str, session_id: &str) -> PathBuf {
    let safe: String = format!("{agent}-{session_id}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    dir.join(format!("{safe}.json"))
}

fn load_state(path: &Path) -> SessionState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &SessionState) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(state) {
        let _ = std::fs::write(path, text);
    }
}

fn prune_old_states(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age.as_secs() > STATE_MAX_AGE_SECS);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ── The briefing call ───────────────────────────────────────────────────────

fn query_component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Files relative to the repository root, forward slashes; files outside it are dropped.
pub fn repo_relative(files: &[String], cwd: Option<&Path>, root: &Path) -> Vec<String> {
    let root = root.to_string_lossy().replace('\\', "/");
    let root = root.trim_end_matches('/').to_string();
    files
        .iter()
        .filter_map(|f| {
            let p = Path::new(f);
            let abs = if p.is_absolute() || f.starts_with('/') {
                PathBuf::from(f)
            } else {
                cwd.map(|c| c.join(p)).unwrap_or_else(|| p.to_path_buf())
            };
            let s = abs.to_string_lossy().replace('\\', "/");
            let rel = s
                .strip_prefix(&format!("{root}/"))
                .map(str::to_string)
                .or_else(|| {
                    (!(p.is_absolute() || f.starts_with('/'))).then(|| f.replace('\\', "/"))
                })?;
            let rel = rel.trim_start_matches("./").to_string();
            (!rel.is_empty() && !rel.starts_with("../")).then_some(rel)
        })
        .collect()
}

struct BriefRequest<'a> {
    repo_url: &'a str,
    mode: &'a str,
    agent: &'a str,
    query: Option<&'a str>,
    paths: &'a [String],
    skip: &'a [String],
}

/// Ask Autter for a briefing; None on any failure.
fn fetch_brief(req: &BriefRequest<'_>) -> Option<Value> {
    let mut context = ApiContext::new(None).with_timeout(BRIEF_TIMEOUT_SECS);
    let token = context.auth_token.clone()?;
    let identity = extract_identity_from_access_token(&token);
    // The org that tracks this repository (multi-org PAT logins), else the active one.
    let org = crate::api::client::resolve_org_for_repo_cached(req.repo_url)
        .and_then(|org_id| {
            identity
                .orgs
                .iter()
                .find(|o| o.org_id.as_deref() == Some(org_id.as_str()))
                .cloned()
        })
        .or_else(|| identity.active_org().cloned())?;
    let slug = org.org_slug.clone()?;
    if org.org_id != identity.active_org_id
        && let Some(org_id) = org.org_id.as_deref()
    {
        context.auth_token = Some(crate::api::client::access_token_for_org(org_id)?);
    }
    let mut url = format!(
        "/api/cli/orgs/{}/repo-memory/brief?repo={}&mode={}&agent={}",
        query_component(&slug),
        query_component(req.repo_url),
        req.mode,
        query_component(req.agent),
    );
    if let Some(q) = req.query {
        let q: String = q.chars().take(1_000).collect();
        url.push_str(&format!("&q={}", query_component(&q)));
    }
    if !req.paths.is_empty() {
        url.push_str(&format!("&paths={}", query_component(&req.paths.join(","))));
    }
    if !req.skip.is_empty() {
        url.push_str(&format!("&skip={}", query_component(&req.skip.join(","))));
    }
    let response = context.get(&url).ok()?;
    if response.status_code != 200 {
        return None;
    }
    serde_json::from_str(response.as_str().ok()?).ok()
}

fn markdown_of(brief: &Option<Value>) -> String {
    brief
        .as_ref()
        .and_then(|b| b.get("markdown"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// A briefing fetcher: (mode, prompt, repo-relative paths, areas to skip) → response.
pub type FetchBrief<'a> = dyn FnMut(&str, Option<&str>, &[String], &[String]) -> Option<Value> + 'a;

/// Everything the hook prints for one call, given a briefing fetcher (tests pass a fake).
pub fn run_hook_with(
    agent: &str,
    call: &HookCall,
    repo_root: &Path,
    allow_prompt_text: bool,
    state: &mut SessionState,
    fetch: &mut FetchBrief<'_>,
) -> Option<String> {
    let dialect = dialect_for(agent)?;
    let mut parts: Vec<String> = Vec::new();
    let mut session = |state: &mut SessionState, parts: &mut Vec<String>| {
        let brief = fetch("session", None, &[], &[]);
        if let Some(area_paths) = area_paths_of(&brief) {
            state.area_paths = Some(area_paths);
        }
        let md = markdown_of(&brief);
        state.session_done = true;
        if !md.is_empty() {
            parts.push(md);
        }
    };
    match call.moment {
        HookMoment::SessionStart => session(state, &mut parts),
        HookMoment::Prompt => {
            // Agents whose prompt output is dropped get nothing here; their
            // session hook carries the briefing.
            if render_output(dialect, HookMoment::Prompt, &call.event, "x").is_none() {
                return None;
            }
            if !state.session_done {
                session(state, &mut parts);
            }
            if allow_prompt_text && let Some(prompt) = call.prompt.as_deref() {
                let md = markdown_of(&fetch("prompt", Some(prompt), &[], &[]));
                if !md.is_empty() {
                    parts.push(md);
                }
            }
        }
        HookMoment::File => {
            let paths = repo_relative(&call.files, call.cwd.as_deref(), repo_root);
            if paths.is_empty() {
                return None;
            }
            // With the area map from the session briefing, only a file in an
            // area not injected yet is worth a call.
            if let Some(area_paths) = state.area_paths.as_deref()
                && new_areas_for(&paths, area_paths, &state.areas).is_empty()
            {
                return None;
            }
            let brief = fetch("file", None, &paths, &state.areas);
            for area in brief
                .as_ref()
                .and_then(|b| b.get("areas"))
                .and_then(|a| a.as_array())
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str())
            {
                if !state.areas.iter().any(|x| x == area) {
                    state.areas.push(area.to_string());
                }
            }
            let md = markdown_of(&brief);
            if !md.is_empty() {
                parts.push(md);
            }
        }
    }
    let joined: String = parts.join("\n\n").chars().take(MAX_CONTEXT_CHARS).collect();
    render_output(dialect, call.moment, &call.event, &joined)
}

/// Entry point: `autter memory-hook <agent> [--event <name>] [--hook-input stdin|<json>]`.
pub fn handle_memory_hook(args: &[String]) {
    let Some(agent) = args.first().map(String::as_str) else {
        eprintln!("Usage: autter memory-hook <agent> [--event <name>] --hook-input stdin");
        std::process::exit(0);
    };
    if dialect_for(agent).is_none() {
        eprintln!("autter memory-hook: no memory injection for agent '{agent}'");
        std::process::exit(0);
    }
    if !crate::config::Config::get()
        .get_feature_flags()
        .memory_context
    {
        std::process::exit(0);
    }
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let input = match flag("--hook-input") {
        Some(v) if v != "stdin" => v,
        _ => {
            let mut buf = String::new();
            if std::io::stdin().read_to_string(&mut buf).is_err() {
                std::process::exit(0);
            }
            buf
        }
    };
    let event = flag("--event");
    let Some(call) = parse_hook_call(input.trim_start_matches('\u{feff}'), event.as_deref()) else {
        std::process::exit(0);
    };
    let Some(cwd) = call.cwd.clone().or_else(|| std::env::current_dir().ok()) else {
        std::process::exit(0);
    };
    let Ok(repo) = crate::git::repository::discover_repository_in_path_no_git_exec(&cwd) else {
        std::process::exit(0);
    };
    let Some(repo_url) = crate::repo_url::resolve_repo_url_from_repo(&repo) else {
        std::process::exit(0);
    };
    let config = crate::config::Config::get();
    let repository = Some(repo);
    if config.has_repository_filters() && !config.is_allowed_repository(&repository) {
        std::process::exit(0);
    }
    let allow_prompt_text = !matches!(
        config.effective_prompt_storage(&repository),
        crate::config::PromptStorageMode::Local
    );
    let Some(Ok(root)) = repository.as_ref().map(|r| r.workdir()) else {
        std::process::exit(0);
    };

    let dir = state_dir();
    let state_file = match (&dir, call.session_id.as_deref()) {
        (Some(dir), Some(id)) => Some(state_path(dir, agent, id)),
        _ => None,
    };
    let mut state = state_file.as_deref().map(load_state).unwrap_or_default();
    let mut fetch = |mode: &str, query: Option<&str>, paths: &[String], skip: &[String]| {
        fetch_brief(&BriefRequest {
            repo_url: &repo_url,
            mode,
            agent,
            query,
            paths,
            skip,
        })
    };
    let output = run_hook_with(
        agent,
        &call,
        &root,
        allow_prompt_text,
        &mut state,
        &mut fetch,
    );
    if let Some(path) = state_file {
        save_state(&path, &state);
        if let Some(dir) = dir
            && call.moment == HookMoment::SessionStart
        {
            prune_old_states(&dir);
        }
    }
    if let Some(out) = output {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
    std::process::exit(0);
}

/// `autter memory [question] [--path <file>]...`: print the repository memory
/// briefing in a terminal. For people, and for agents whose hooks cannot add
/// context (the bundled `autter-memory` skill tells them to run it before
/// editing): no arguments prints the session briefing (the map), a question
/// prints the matching sections, `--path` prints the area pages for files.
pub fn handle_memory_command(args: &[String]) {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("Usage: autter memory [question] [--path <file>]...");
        println!();
        println!("Print what Autter's repository memory says about this repository:");
        println!(
            "  autter memory                                   the map: conventions, gotchas, area pages"
        );
        println!("  autter memory \"how are refunds retried\"         the sections that match");
        println!("  autter memory --path src/payments/refund.ts     the area page for a file");
        return;
    }
    let mut paths: Vec<String> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--path" || args[i] == "-p" {
            if let Some(p) = args.get(i + 1) {
                paths.push(p.clone());
            }
            i += 2;
            continue;
        }
        words.push(args[i].clone());
        i += 1;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Ok(repo) = crate::git::repository::discover_repository_in_path_no_git_exec(&cwd) else {
        eprintln!("autter memory: run this inside a git repository.");
        std::process::exit(1);
    };
    let Some(repo_url) = crate::repo_url::resolve_repo_url_from_repo(&repo) else {
        eprintln!("autter memory: this repository has no remote Autter can match.");
        std::process::exit(1);
    };
    let Ok(root) = repo.workdir() else {
        std::process::exit(1);
    };
    if ApiContext::new(None).auth_token.is_none() {
        eprintln!("autter memory: sign in first with `autter login`.");
        std::process::exit(1);
    }
    let rel = repo_relative(&paths, Some(&cwd), &root);
    let question = words.join(" ");
    let (mode, query) = if !rel.is_empty() {
        ("file", None)
    } else if !question.trim().is_empty() {
        ("prompt", Some(question.as_str()))
    } else {
        ("session", None)
    };
    let brief = fetch_brief(&BriefRequest {
        repo_url: &repo_url,
        mode,
        agent: "cli",
        query,
        paths: &rel,
        skip: &[],
    });
    let markdown = markdown_of(&brief);
    if brief.is_none() {
        eprintln!("autter memory: could not reach Autter. Try again, or check `autter whoami`.");
        std::process::exit(1);
    }
    if markdown.is_empty() {
        println!(
            "{}",
            match mode {
                "file" => "No repository memory covers these files yet.",
                "prompt" => "Nothing in repository memory matches that yet.",
                _ =>
                    "This repository has no memory yet. Autter writes it overnight from sessions, reviews and production.",
            }
        );
        return;
    }
    println!("{markdown}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn call(json: Value) -> Option<HookCall> {
        parse_hook_call(&json.to_string(), None)
    }

    #[test]
    fn reads_session_prompt_and_file_hooks() {
        let s = call(json!({"hook_event_name": "SessionStart", "session_id": "s1", "cwd": "/repo", "source": "startup"})).unwrap();
        assert_eq!(s.moment, HookMoment::SessionStart);
        assert_eq!(s.session_id.as_deref(), Some("s1"));
        let p = call(json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "cwd": "/repo", "prompt": "fix retries"})).unwrap();
        assert_eq!(
            (p.moment, p.prompt.as_deref()),
            (HookMoment::Prompt, Some("fix retries"))
        );
        let g = call(json!({"hook_event_name": "BeforeAgent", "session_id": "g", "cwd": "/repo", "prompt": "hi"})).unwrap();
        assert_eq!(g.moment, HookMoment::Prompt);
        let f = call(json!({"hook_event_name": "PostToolUse", "tool_name": "Read", "session_id": "s1", "cwd": "/repo",
                            "tool_input": {"file_path": "/repo/src/payments/refund.ts"}})).unwrap();
        assert_eq!(
            (f.moment, f.files.clone()),
            (
                HookMoment::File,
                vec!["/repo/src/payments/refund.ts".to_string()]
            )
        );
        let m = call(json!({"hook_event_name": "PostToolUse", "tool_name": "MultiEdit", "cwd": "/repo",
                            "tool_input": {"edits": [{"file_path": "a.ts"}, {"file_path": "b.ts"}]}})).unwrap();
        assert_eq!(m.files, vec!["a.ts".to_string(), "b.ts".to_string()]);
        // Tools that touch no file, and other events, are not ours.
        assert!(call(json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": "ls"}})).is_none());
        assert!(call(json!({"hook_event_name": "Stop", "session_id": "s1"})).is_none());
        assert!(parse_hook_call("not json", None).is_none());
    }

    #[test]
    fn reads_codex_patches_copilot_arguments_and_the_event_flag() {
        let codex = call(json!({"hook_event_name": "PreToolUse", "tool_name": "apply_patch", "cwd": "/repo",
            "tool_input": {"command": ["apply_patch", "*** Begin Patch\n*** Update File: src/a.ts\n@@\n*** Add File: src/b.ts\n*** End Patch"]}}))
        .unwrap();
        assert_eq!(
            codex.files,
            vec!["src/a.ts".to_string(), "src/b.ts".to_string()]
        );
        // Copilot: camelCase payload, arguments as a JSON string, event from the command line.
        let copilot = parse_hook_call(
            &json!({"cwd": "/repo", "toolName": "edit", "toolArgs": "{\"path\": \"src/c.ts\"}"})
                .to_string(),
            Some("postToolUse"),
        )
        .unwrap();
        assert_eq!(
            (copilot.moment, copilot.files),
            (HookMoment::File, vec!["src/c.ts".to_string()])
        );
        let session =
            parse_hook_call(&json!({"cwd": "/repo"}).to_string(), Some("sessionStart")).unwrap();
        assert_eq!(session.moment, HookMoment::SessionStart);
        // Plugins call with plain moment names.
        let plugin = parse_hook_call(
            &json!({"cwd": "/repo", "session_id": "x", "prompt": "hi"}).to_string(),
            Some("prompt"),
        )
        .unwrap();
        assert_eq!(plugin.moment, HookMoment::Prompt);
    }

    #[test]
    fn skips_the_call_for_files_outside_new_areas() {
        let areas = vec![
            AreaPath {
                slug: "areas/src".into(),
                path: "src/".into(),
            },
            AreaPath {
                slug: "areas/src-payments".into(),
                path: "src/payments/".into(),
            },
        ];
        assert_eq!(
            new_areas_for(
                &[
                    "src/payments/a.ts".into(),
                    "src/x.ts".into(),
                    "README.md".into()
                ],
                &areas,
                &[]
            ),
            vec!["areas/src-payments".to_string(), "areas/src".to_string()]
        );
        assert!(
            new_areas_for(
                &["src/payments/a.ts".into()],
                &areas,
                &["areas/src-payments".into()]
            )
            .is_empty()
        );
        assert!(new_areas_for(&["docs/a.md".into()], &areas, &[]).is_empty());

        // The session briefing carries the area map; a later read outside a new
        // area makes no call.
        let root = Path::new("/repo");
        let mut state = SessionState::default();
        let calls = RefCell::new(0);
        let mut fetch = |mode: &str, _q: Option<&str>, _p: &[String], _s: &[String]| {
            *calls.borrow_mut() += 1;
            match mode {
                "session" => Some(
                    json!({"markdown": "map", "areaPaths": [{"slug": "areas/src-payments", "path": "src/payments/"}]}),
                ),
                _ => Some(json!({"markdown": "area page", "areas": ["areas/src-payments"]})),
            }
        };
        let start =
            call(json!({"hook_event_name": "SessionStart", "session_id": "s", "cwd": "/repo"}))
                .unwrap();
        run_hook_with("claude", &start, root, true, &mut state, &mut fetch).unwrap();
        let read = |file: &str| {
            call(json!({"hook_event_name": "PostToolUse", "tool_name": "Read", "session_id": "s", "cwd": "/repo",
                        "tool_input": {"file_path": file}})).unwrap()
        };
        assert!(
            run_hook_with(
                "claude",
                &read("/repo/README.md"),
                root,
                true,
                &mut state,
                &mut fetch
            )
            .is_none()
        );
        assert_eq!(*calls.borrow(), 1, "no call for a file outside every area");
        assert!(
            run_hook_with(
                "claude",
                &read("/repo/src/payments/a.ts"),
                root,
                true,
                &mut state,
                &mut fetch
            )
            .is_some()
        );
        assert_eq!(*calls.borrow(), 2);
        assert!(
            run_hook_with(
                "claude",
                &read("/repo/src/payments/b.ts"),
                root,
                true,
                &mut state,
                &mut fetch
            )
            .is_none()
        );
        assert_eq!(*calls.borrow(), 2, "no call once the area was injected");
    }

    #[test]
    fn makes_files_repo_relative() {
        let root = Path::new("/repo");
        assert_eq!(
            repo_relative(
                &[
                    "/repo/src/a.ts".into(),
                    "lib/b.ts".into(),
                    "/elsewhere/c.ts".into(),
                    "./d.ts".into()
                ],
                Some(Path::new("/repo")),
                root
            ),
            vec![
                "src/a.ts".to_string(),
                "lib/b.ts".to_string(),
                "d.ts".to_string()
            ]
        );
    }

    #[test]
    fn renders_each_agents_output_shape() {
        let parse = |s: String| serde_json::from_str::<Value>(&s).unwrap();
        let claude = parse(
            render_output(
                HookDialect::ClaudeJson,
                HookMoment::SessionStart,
                "SessionStart",
                "m",
            )
            .unwrap(),
        );
        assert_eq!(
            claude["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
        assert_eq!(claude["hookSpecificOutput"]["additionalContext"], "m");
        let gemini = parse(
            render_output(
                HookDialect::GeminiJson,
                HookMoment::Prompt,
                "BeforeAgent",
                "m",
            )
            .unwrap(),
        );
        assert_eq!(
            gemini,
            json!({"hookSpecificOutput": {"additionalContext": "m"}})
        );
        assert_eq!(
            render_output(
                HookDialect::Droid,
                HookMoment::Prompt,
                "UserPromptSubmit",
                "m"
            )
            .as_deref(),
            Some("m")
        );
        let droid_file =
            parse(render_output(HookDialect::Droid, HookMoment::File, "PostToolUse", "m").unwrap());
        assert_eq!(droid_file["hookSpecificOutput"]["additionalContext"], "m");
        assert_eq!(
            parse(
                render_output(
                    HookDialect::Copilot,
                    HookMoment::SessionStart,
                    "SessionStart",
                    "m"
                )
                .unwrap()
            ),
            json!({"additionalContext": "m", "hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": "m"}})
        );
        assert!(
            render_output(
                HookDialect::Copilot,
                HookMoment::Prompt,
                "userPromptSubmitted",
                "m"
            )
            .is_none()
        );
        assert_eq!(
            parse(
                render_output(HookDialect::Cursor, HookMoment::File, "postToolUse", "m").unwrap()
            ),
            json!({"additional_context": "m"})
        );
        assert_eq!(
            render_output(HookDialect::Plain, HookMoment::Prompt, "prompt", "m").as_deref(),
            Some("m")
        );
        assert!(
            render_output(
                HookDialect::ClaudeJson,
                HookMoment::SessionStart,
                "SessionStart",
                "  "
            )
            .is_none()
        );
        assert!(dialect_for("windsurf").is_none());
    }

    #[test]
    fn injects_the_session_once_matching_sections_per_prompt_and_each_area_once() {
        let root = Path::new("/repo");
        let mut state = SessionState::default();
        let calls: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let mut fetch = |mode: &str, q: Option<&str>, paths: &[String], skip: &[String]| {
            calls.borrow_mut().push(format!(
                "{mode}|{}|{}|{}",
                q.unwrap_or(""),
                paths.join(","),
                skip.join(",")
            ));
            match mode {
                "file" if skip.contains(&"areas/src-payments".to_string()) => {
                    Some(json!({"markdown": "", "areas": []}))
                }
                "file" => Some(json!({"markdown": "file memory", "areas": ["areas/src-payments"]})),
                m => Some(json!({"markdown": format!("{m} memory"), "areas": []})),
            }
        };
        let prompt = call(json!({"hook_event_name": "UserPromptSubmit", "session_id": "s", "cwd": "/repo", "prompt": "fix refunds"})).unwrap();
        // First prompt without a session-start hook: session briefing, then the matches.
        let out = run_hook_with("claude", &prompt, root, true, &mut state, &mut fetch).unwrap();
        assert!(out.contains("session memory\\n\\nprompt memory"));
        // Later prompts: matches only.
        let out = run_hook_with("claude", &prompt, root, true, &mut state, &mut fetch).unwrap();
        assert!(!out.contains("session memory"));
        // Local prompt storage: the prompt text is never sent.
        let prompts_sent = || {
            calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("prompt|"))
                .count()
        };
        let before = prompts_sent();
        assert!(run_hook_with("claude", &prompt, root, false, &mut state, &mut fetch).is_none());
        assert_eq!(prompts_sent(), before);

        let read = call(json!({"hook_event_name": "PostToolUse", "tool_name": "Read", "session_id": "s", "cwd": "/repo",
                               "tool_input": {"file_path": "/repo/src/payments/refund.ts"}})).unwrap();
        let out = run_hook_with("claude", &read, root, true, &mut state, &mut fetch).unwrap();
        assert!(out.contains("file memory"));
        assert!(out.contains("\"hookEventName\":\"PostToolUse\""));
        assert_eq!(state.areas, vec!["areas/src-payments".to_string()]);
        // The same area again: asked with it skipped, nothing injected.
        assert!(run_hook_with("claude", &read, root, true, &mut state, &mut fetch).is_none());
        assert_eq!(
            calls.borrow().last().unwrap(),
            "file||src/payments/refund.ts|areas/src-payments"
        );
        // Copilot drops prompt output, so nothing is fetched for it.
        let before = calls.borrow().len();
        assert!(
            run_hook_with(
                "github-copilot",
                &prompt,
                root,
                true,
                &mut SessionState::default(),
                &mut fetch
            )
            .is_none()
        );
        assert_eq!(calls.borrow().len(), before);
        // Agents without a context hook get nothing.
        assert!(run_hook_with("windsurf", &read, root, true, &mut state, &mut fetch).is_none());
    }
}
