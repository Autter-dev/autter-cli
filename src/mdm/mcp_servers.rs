//! Register (or remove) Autter's MCP server in each coding agent's user-level
//! MCP configuration, for `autter mcp connect|disconnect`.
//!
//! The entry is named `autter` and carries only the server URL: the server
//! authenticates with OAuth, so each agent signs the person in on first use
//! and no token is ever written. Formats, from each agent's documentation
//! (checked 2026-11-03):
//!
//! | Agent | File | Entry |
//! |---|---|---|
//! | Claude Code | `claude mcp add --transport http --scope user` when `claude` is on PATH, else `~/.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json` when set) | `mcpServers.autter = {"type":"http","url":…}` |
//! | Codex CLI | `codex mcp add autter --url` when `codex` is on PATH, else `~/.codex/config.toml` | `[mcp_servers.autter] url = "…"` |
//! | Cursor | `~/.cursor/mcp.json` | `mcpServers.autter = {"url":…}` |
//! | VS Code | `<user dir>/mcp.json` (and each profile's existing `mcp.json`) | `servers.autter = {"type":"http","url":…}` |
//! | Gemini CLI | `~/.gemini/settings.json` | `mcpServers.autter = {"httpUrl":…}` |
//! | OpenCode | `~/.config/opencode/opencode.json` (or an existing `opencode.jsonc`) | `mcp.autter = {"type":"remote","url":…,"enabled":true}` |
//!
//! JSON files are edited through the jsonc concrete syntax tree, so comments,
//! key order and indentation stay as written; `config.toml` through
//! `toml_edit`, for the same reason. Every file is backed up before it is
//! written, and an existing `autter` entry is replaced, never duplicated.

use crate::error::AutterError;
use crate::mdm::utils::{
    backup_file, binary_exists, claude_config_dir, generate_diff, home_dir, resolve_editor_cli,
    write_atomic,
};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The entry's name in every agent's configuration.
pub const SERVER_NAME: &str = "autter";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAgent {
    ClaudeCode,
    Codex,
    Cursor,
    VsCode,
    Gemini,
    OpenCode,
}

impl McpAgent {
    pub const ALL: [McpAgent; 6] = [
        McpAgent::ClaudeCode,
        McpAgent::Codex,
        McpAgent::Cursor,
        McpAgent::VsCode,
        McpAgent::Gemini,
        McpAgent::OpenCode,
    ];

    /// The name `--agent` takes.
    pub fn id(self) -> &'static str {
        match self {
            McpAgent::ClaudeCode => "claude",
            McpAgent::Codex => "codex",
            McpAgent::Cursor => "cursor",
            McpAgent::VsCode => "vscode",
            McpAgent::Gemini => "gemini",
            McpAgent::OpenCode => "opencode",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            McpAgent::ClaudeCode => "Claude Code",
            McpAgent::Codex => "Codex",
            McpAgent::Cursor => "Cursor",
            McpAgent::VsCode => "VS Code",
            McpAgent::Gemini => "Gemini CLI",
            McpAgent::OpenCode => "OpenCode",
        }
    }

    /// An agent by the name a person types (`--agent`).
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claudecode" => McpAgent::ClaudeCode,
            "codex" | "codex-cli" => McpAgent::Codex,
            "cursor" => McpAgent::Cursor,
            "vscode" | "vs-code" | "code" => McpAgent::VsCode,
            "gemini" | "gemini-cli" => McpAgent::Gemini,
            "opencode" | "open-code" => McpAgent::OpenCode,
            _ => return None,
        })
    }

    /// The agent's own CLI that manages its MCP servers, when it has one.
    fn cli(self) -> Option<&'static str> {
        match self {
            McpAgent::ClaudeCode => Some("claude"),
            McpAgent::Codex => Some("codex"),
            _ => None,
        }
    }

    /// How the person signs in to Autter from this agent.
    pub fn sign_in_hint(self) -> &'static str {
        match self {
            McpAgent::ClaudeCode => "sign in: run /mcp in Claude Code and choose autter",
            McpAgent::Codex => "sign in: codex mcp login autter",
            McpAgent::Cursor => "Cursor asks you to sign in to Autter the first time it connects",
            McpAgent::VsCode => {
                "VS Code asks you to sign in to Autter when the server first starts"
            }
            McpAgent::Gemini => "sign in: /mcp auth autter in Gemini CLI",
            McpAgent::OpenCode => {
                "OpenCode asks you to sign in on first use (or run: opencode mcp auth autter)"
            }
        }
    }

    /// Installed: its binary is on PATH (or, for an editor, the app is there)
    /// or its configuration directory exists. Mirrors the hook installers.
    pub fn installed(self, env: &McpEnv) -> bool {
        let on_path = env.on_path;
        let home = &env.home;
        match self {
            McpAgent::ClaudeCode => {
                on_path("claude") || env.claude_dir.exists() || env.claude_json.exists()
            }
            McpAgent::Codex => on_path("codex") || home.join(".codex").exists(),
            McpAgent::Cursor => {
                on_path("cursor")
                    || (env.editor_installed)("cursor")
                    || home.join(".cursor").exists()
            }
            McpAgent::VsCode => {
                on_path("code")
                    || (env.editor_installed)("code")
                    || home.join(".vscode").exists()
                    || env.vscode_user_dir.exists()
            }
            McpAgent::Gemini => on_path("gemini") || home.join(".gemini").exists(),
            McpAgent::OpenCode => on_path("opencode") || env.opencode_dir.exists(),
        }
    }

    /// The user-level files that hold the agent's MCP servers.
    pub fn config_files(self, env: &McpEnv) -> Vec<PathBuf> {
        match self {
            McpAgent::ClaudeCode => vec![env.claude_json.clone()],
            McpAgent::Codex => vec![env.home.join(".codex").join("config.toml")],
            McpAgent::Cursor => vec![env.home.join(".cursor").join("mcp.json")],
            McpAgent::VsCode => {
                // The default profile's file, and the file of each profile
                // that already keeps its own MCP servers.
                let mut files = vec![env.vscode_user_dir.join("mcp.json")];
                if let Ok(entries) = fs::read_dir(env.vscode_user_dir.join("profiles")) {
                    let mut profiles: Vec<PathBuf> = entries
                        .flatten()
                        .map(|entry| entry.path().join("mcp.json"))
                        .filter(|path| path.is_file())
                        .collect();
                    profiles.sort();
                    files.extend(profiles);
                }
                files
            }
            McpAgent::Gemini => vec![env.home.join(".gemini").join("settings.json")],
            McpAgent::OpenCode => {
                let json = env.opencode_dir.join("opencode.json");
                let jsonc = env.opencode_dir.join("opencode.jsonc");
                vec![if !json.exists() && jsonc.exists() {
                    jsonc
                } else {
                    json
                }]
            }
        }
    }

    /// The entry this agent's files carry for Autter.
    fn format(self, url: &str) -> Format {
        let entry = |pairs: &[(&'static str, Value)]| Format::Json {
            parent: match self {
                McpAgent::VsCode => "servers",
                McpAgent::OpenCode => "mcp",
                _ => "mcpServers",
            },
            entry: pairs.to_vec(),
        };
        match self {
            McpAgent::ClaudeCode | McpAgent::VsCode => {
                entry(&[("type", json!("http")), ("url", json!(url))])
            }
            McpAgent::Codex => Format::Toml,
            McpAgent::Cursor => entry(&[("url", json!(url))]),
            McpAgent::Gemini => entry(&[("httpUrl", json!(url))]),
            McpAgent::OpenCode => entry(&[
                ("type", json!("remote")),
                ("url", json!(url)),
                ("enabled", json!(true)),
            ]),
        }
    }

    /// The agent's own command that adds the server.
    pub fn cli_add(self, url: &str) -> Option<Vec<String>> {
        let argv: &[&str] = match self {
            McpAgent::ClaudeCode => &[
                "claude",
                "mcp",
                "add",
                "--transport",
                "http",
                "--scope",
                "user",
                SERVER_NAME,
                url,
            ],
            McpAgent::Codex => &["codex", "mcp", "add", SERVER_NAME, "--url", url],
            _ => return None,
        };
        Some(argv.iter().map(|s| s.to_string()).collect())
    }

    /// The agent's own command that removes the server.
    pub fn cli_remove(self) -> Option<Vec<String>> {
        let argv: &[&str] = match self {
            McpAgent::ClaudeCode => &["claude", "mcp", "remove", SERVER_NAME, "--scope", "user"],
            McpAgent::Codex => &["codex", "mcp", "remove", SERVER_NAME],
            _ => return None,
        };
        Some(argv.iter().map(|s| s.to_string()).collect())
    }
}

/// Where agents keep their configuration, and how to look for programs; tests
/// point all of it at a temporary directory.
pub struct McpEnv {
    pub home: PathBuf,
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude_dir: PathBuf,
    /// Claude Code's user configuration (user-scope MCP servers).
    pub claude_json: PathBuf,
    /// VS Code's `User` directory (default profile).
    pub vscode_user_dir: PathBuf,
    /// OpenCode's global configuration directory.
    pub opencode_dir: PathBuf,
    /// Whether a program is on PATH (and so whether an agent's CLI is used).
    pub on_path: fn(&str) -> bool,
    /// Whether an editor app is installed even without its CLI on PATH.
    pub editor_installed: fn(&str) -> bool,
}

fn non_empty_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn vscode_user_dir(home: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        non_empty_env("APPDATA")
            .unwrap_or_else(|| home.join("AppData").join("Roaming"))
            .join("Code")
            .join("User")
    }
    #[cfg(target_os = "macos")]
    {
        home.join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        home.join(".config").join("Code").join("User")
    }
}

impl McpEnv {
    /// This machine's locations.
    pub fn system() -> Self {
        let home = home_dir();
        let claude_dir = claude_config_dir();
        let claude_json = if non_empty_env("CLAUDE_CONFIG_DIR").is_some() {
            claude_dir.join(".claude.json")
        } else {
            home.join(".claude.json")
        };
        let opencode_dir = non_empty_env("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("opencode");
        Self {
            vscode_user_dir: vscode_user_dir(&home),
            home,
            claude_dir,
            claude_json,
            opencode_dir,
            on_path: binary_exists,
            editor_installed: |name| resolve_editor_cli(name).is_some(),
        }
    }
}

/// How an agent's file holds the entry.
#[derive(Debug, Clone)]
enum Format {
    /// `<parent>.autter = {entry}` in a JSON (or JSONC) object.
    Json {
        parent: &'static str,
        entry: Vec<(&'static str, Value)>,
    },
    /// `[mcp_servers.autter] url = "…"` in TOML.
    Toml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Connect,
    Disconnect,
}

/// The `autter` entry in a file, compared with the one Autter writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    Absent,
    Same,
    Different,
}

fn to_cst(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(to_cst).collect()),
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), to_cst(value)))
                .collect(),
        ),
    }
}

fn parse_json(text: &str) -> Result<CstRootNode, String> {
    let input = if text.trim().is_empty() { "{}" } else { text };
    let root = CstRootNode::parse(input, &ParseOptions::default()).map_err(|e| e.to_string())?;
    if let Some(value) = root.value()
        && value.as_object().is_none()
    {
        return Err("is not a JSON object".into());
    }
    Ok(root)
}

/// `<parent>.<name>` as written, if present.
fn json_entry(text: &str, parent: &str, name: &str) -> Result<Option<Value>, String> {
    let root = parse_json(text)?;
    let Some(object) = root.object_value() else {
        return Ok(None);
    };
    Ok(object
        .get(parent)
        .and_then(|prop| prop.value())
        .and_then(|value| value.as_object())
        .and_then(|servers| servers.get(name))
        .and_then(|prop| prop.value())
        .and_then(|value| value.to_serde_value()))
}

/// Set `<parent>.<name>` to `entry`, keeping everything else as written.
/// None when it already holds exactly that.
fn json_with_entry(
    text: &str,
    parent: &str,
    name: &str,
    entry: &[(&'static str, Value)],
) -> Result<Option<String>, String> {
    let desired = Value::Object(
        entry
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect(),
    );
    let input = CstInputValue::Object(
        entry
            .iter()
            .map(|(key, value)| (key.to_string(), to_cst(value)))
            .collect(),
    );
    let root = parse_json(text)?;
    let object = root.object_value_or_set();
    match object.get(parent) {
        Some(prop) => {
            let servers = prop
                .value()
                .and_then(|value| value.as_object())
                .ok_or_else(|| format!("`{parent}` is not an object"))?;
            match servers.get(name) {
                Some(existing) => {
                    if existing.value().and_then(|v| v.to_serde_value()) == Some(desired) {
                        return Ok(None);
                    }
                    existing.set_value(input);
                }
                None => {
                    servers.append(name, input);
                }
            }
        }
        None => {
            object.append(
                parent,
                CstInputValue::Object(vec![(name.to_string(), input)]),
            );
        }
    }
    let mut out = root.to_string();
    if !out.ends_with('\n') && (text.is_empty() || text.ends_with('\n')) {
        out.push('\n');
    }
    Ok(Some(out))
}

/// Remove `<parent>.<name>`, keeping everything else as written. None when absent.
fn json_without_entry(text: &str, parent: &str, name: &str) -> Result<Option<String>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let root = parse_json(text)?;
    let Some(prop) = root
        .object_value()
        .and_then(|object| object.get(parent))
        .and_then(|prop| prop.value())
        .and_then(|value| value.as_object())
        .and_then(|servers| servers.get(name))
    else {
        return Ok(None);
    };
    prop.remove();
    Ok(Some(root.to_string()))
}

fn parse_toml(text: &str) -> Result<toml_edit::DocumentMut, String> {
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|e| e.to_string().trim().to_string())
}

fn toml_entry_state(text: &str, name: &str, url: &str) -> Result<EntryState, String> {
    let doc = parse_toml(text)?;
    let Some(entry) = doc
        .get("mcp_servers")
        .and_then(|item| item.as_table_like())
        .and_then(|servers| servers.get(name))
    else {
        return Ok(EntryState::Absent);
    };
    let same = entry.as_table_like().is_some_and(|table| {
        table.len() == 1 && table.get("url").and_then(|u| u.as_str()) == Some(url)
    });
    Ok(if same {
        EntryState::Same
    } else {
        EntryState::Different
    })
}

/// Set `[mcp_servers.<name>]` to just `url`, keeping everything else as
/// written. None when it already holds exactly that.
fn toml_with_server(text: &str, name: &str, url: &str) -> Result<Option<String>, String> {
    if toml_entry_state(text, name, url)? == EntryState::Same {
        return Ok(None);
    }
    let mut doc = parse_toml(text)?;
    let servers = doc.entry("mcp_servers").or_insert_with(|| {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        toml_edit::Item::Table(table)
    });
    let servers = servers
        .as_table_like_mut()
        .ok_or_else(|| "`mcp_servers` is not a table".to_string())?;
    if let Some(toml_edit::Item::Table(existing)) = servers.get_mut(name) {
        // Replace in place, so the section stays where it was.
        existing.clear();
        existing.insert("url", toml_edit::value(url));
    } else {
        let mut table = toml_edit::Table::new();
        table.insert("url", toml_edit::value(url));
        servers.insert(name, toml_edit::Item::Table(table));
    }
    Ok(Some(doc.to_string()))
}

/// Remove `[mcp_servers.<name>]`. None when absent.
fn toml_without_server(text: &str, name: &str) -> Result<Option<String>, String> {
    let mut doc = parse_toml(text)?;
    let removed = doc
        .get_mut("mcp_servers")
        .and_then(|item| item.as_table_like_mut())
        .and_then(|servers| servers.remove(name))
        .is_some();
    Ok(removed.then(|| doc.to_string()))
}

fn entry_state(format: &Format, text: &str, url: &str) -> Result<EntryState, String> {
    match format {
        Format::Json { parent, entry } => {
            let desired = Value::Object(
                entry
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.clone()))
                    .collect(),
            );
            Ok(match json_entry(text, parent, SERVER_NAME)? {
                None => EntryState::Absent,
                Some(value) if value == desired => EntryState::Same,
                Some(_) => EntryState::Different,
            })
        }
        Format::Toml => toml_entry_state(text, SERVER_NAME, url),
    }
}

/// The file's new text for `action`, or None when it needs no change.
fn edited(
    format: &Format,
    action: Action,
    text: &str,
    url: &str,
) -> Result<Option<String>, String> {
    match (format, action) {
        (Format::Json { parent, entry }, Action::Connect) => {
            json_with_entry(text, parent, SERVER_NAME, entry)
        }
        (Format::Json { parent, .. }, Action::Disconnect) => {
            json_without_entry(text, parent, SERVER_NAME)
        }
        (Format::Toml, Action::Connect) => toml_with_server(text, SERVER_NAME, url),
        (Format::Toml, Action::Disconnect) => toml_without_server(text, SERVER_NAME),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Connected,
    AlreadyConnected,
    WouldConnect,
    Disconnected,
    NotConnected,
    WouldDisconnect,
    NotInstalled,
    Failed,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Connected => "connected",
            Status::AlreadyConnected => "already connected",
            Status::WouldConnect => "would connect",
            Status::Disconnected => "disconnected",
            Status::NotConnected => "not connected",
            Status::WouldDisconnect => "would disconnect",
            Status::NotInstalled => "not installed",
            Status::Failed => "failed",
        }
    }
}

/// What `connect` or `disconnect` did for one agent.
#[derive(Debug, Clone, Serialize)]
pub struct AgentReport {
    pub agent: &'static str,
    pub name: &'static str,
    pub status: Status,
    /// The commands run (or that would run) and the files changed (or not).
    pub via: Vec<String>,
    pub backups: Vec<PathBuf>,
    pub notes: Vec<String>,
    /// For `--dry-run`: the planned file changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// Runs an agent's CLI; `Err` carries why it failed.
pub type Runner<'a> = dyn FnMut(&[String]) -> Result<(), String> + 'a;

/// Run a command, quietly, for an agent's own MCP management.
pub fn run_command(argv: &[String]) -> Result<(), String> {
    let (program, args) = argv.split_first().ok_or("no command")?;
    // npm-installed CLIs are `.cmd` shims on Windows, which only cmd runs.
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(program).args(args);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new(program);
        command.args(args);
        command
    };
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run {program}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    Err(text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("exited with {}", output.status)))
}

/// Characters that would need quoting for a shell or `cmd /C`.
fn cli_safe(url: &str) -> bool {
    !url.chars()
        .any(|c| c.is_whitespace() || "&|<>^\"'%`$;()".contains(c))
}

fn command_text(argv: &[String]) -> String {
    argv.join(" ")
}

impl AgentReport {
    fn new(agent: McpAgent, status: Status) -> Self {
        Self {
            agent: agent.id(),
            name: agent.name(),
            status,
            via: Vec::new(),
            backups: Vec::new(),
            notes: Vec::new(),
            diff: None,
        }
    }
}

/// Connect or disconnect one agent. `run` executes the agent's own CLI.
pub fn apply(
    agent: McpAgent,
    action: Action,
    url: &str,
    env: &McpEnv,
    dry_run: bool,
    run: &mut Runner<'_>,
) -> AgentReport {
    if !agent.installed(env) {
        return AgentReport::new(agent, Status::NotInstalled);
    }
    let format = agent.format(url);
    let files = agent.config_files(env);
    let mut notes = Vec::new();
    let mut backups = Vec::new();

    if let (Some(cli), Some(file)) = (agent.cli(), files.first())
        && (env.on_path)(cli)
        && cli_safe(url)
    {
        let state = fs::read_to_string(file)
            .ok()
            .map(|text| entry_state(&format, &text, url));
        match cli_step(agent, action, url, file, state, dry_run, run) {
            Ok(report) => return report,
            Err((failure, made)) => {
                notes.push(failure);
                backups.extend(made);
            }
        }
    }

    let mut report = edit_files(agent, action, url, &format, &files, dry_run);
    notes.append(&mut report.notes);
    report.notes = notes;
    backups.append(&mut report.backups);
    report.backups = backups;
    report
}

/// The agent's own CLI does the change. `Err` (a note, and any backup made)
/// means "edit the file instead".
fn cli_step(
    agent: McpAgent,
    action: Action,
    url: &str,
    file: &Path,
    state: Option<Result<EntryState, String>>,
    dry_run: bool,
    run: &mut Runner<'_>,
) -> Result<AgentReport, (String, Vec<PathBuf>)> {
    let (Some(add), Some(remove)) = (agent.cli_add(url), agent.cli_remove()) else {
        return Err((format!("{} has no MCP command", agent.name()), Vec::new()));
    };
    // Unreadable or unparsable: let the CLI decide.
    let state = match state {
        None => EntryState::Absent,
        Some(Ok(state)) => state,
        Some(Err(_)) => EntryState::Different,
    };
    let commands: Vec<Vec<String>> = match (action, state) {
        (Action::Connect, EntryState::Same) => {
            let mut report = AgentReport::new(agent, Status::AlreadyConnected);
            report.via.push(file.display().to_string());
            report.notes.push(agent.sign_in_hint().to_string());
            return Ok(report);
        }
        (Action::Disconnect, EntryState::Absent) => {
            let mut report = AgentReport::new(agent, Status::NotConnected);
            report.via.push(file.display().to_string());
            return Ok(report);
        }
        (Action::Connect, EntryState::Absent) => vec![add],
        (Action::Connect, EntryState::Different) => vec![remove, add],
        (Action::Disconnect, _) => vec![remove],
    };
    let (done, planned) = match action {
        Action::Connect => (Status::Connected, Status::WouldConnect),
        Action::Disconnect => (Status::Disconnected, Status::WouldDisconnect),
    };
    if dry_run {
        let mut report = AgentReport::new(agent, planned);
        report.via = commands
            .iter()
            .map(|argv| format!("would run: {}", command_text(argv)))
            .collect();
        return Ok(report);
    }
    let mut report = AgentReport::new(agent, done);
    match backup_file(file) {
        Ok(Some(backup)) => report.backups.push(backup),
        Ok(None) => {}
        Err(error) => {
            let mut failed = AgentReport::new(agent, Status::Failed);
            failed.notes.push(error.to_string());
            return Ok(failed);
        }
    }
    let last = commands.len() - 1;
    for (index, argv) in commands.iter().enumerate() {
        match run(argv) {
            Ok(()) => report.via.push(format!("ran: {}", command_text(argv))),
            // Removing an old entry before adding ours may fail harmlessly.
            Err(_) if index < last => {}
            Err(error) => {
                return Err((
                    format!(
                        "`{}` failed ({error}); edited {} instead",
                        command_text(argv),
                        file.display()
                    ),
                    std::mem::take(&mut report.backups),
                ));
            }
        }
    }
    if action == Action::Connect {
        report.notes.push(agent.sign_in_hint().to_string());
    }
    Ok(report)
}

/// Edit each of the agent's files directly.
fn edit_files(
    agent: McpAgent,
    action: Action,
    url: &str,
    format: &Format,
    files: &[PathBuf],
    dry_run: bool,
) -> AgentReport {
    let mut report = AgentReport::new(agent, Status::NotConnected);
    let mut changed = false;
    let mut diffs = Vec::new();
    for file in files {
        let text = match fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if action == Action::Disconnect {
                    continue;
                }
                String::new()
            }
            Err(error) => {
                report.status = Status::Failed;
                report.notes.push(format!("{}: {error}", file.display()));
                continue;
            }
        };
        let new_text = match edited(format, action, &text, url) {
            Ok(Some(new_text)) => new_text,
            Ok(None) => {
                report.via.push(file.display().to_string());
                continue;
            }
            Err(error) => {
                report.status = Status::Failed;
                report
                    .notes
                    .push(format!("{}: {error}; left unchanged", file.display()));
                continue;
            }
        };
        report.via.push(file.display().to_string());
        changed = true;
        if dry_run {
            diffs.push(generate_diff(file, &text, &new_text));
            continue;
        }
        let written = backup_file(file).and_then(|backup| {
            report.backups.extend(backup);
            write_atomic(file, new_text.as_bytes())
        });
        if let Err(error) = written {
            report.status = Status::Failed;
            report.notes.push(error.to_string());
        }
    }
    if report.status == Status::Failed {
        return report;
    }
    report.status = match (action, changed, dry_run) {
        (Action::Connect, true, false) => Status::Connected,
        (Action::Connect, true, true) => Status::WouldConnect,
        (Action::Connect, false, _) => Status::AlreadyConnected,
        (Action::Disconnect, true, false) => Status::Disconnected,
        (Action::Disconnect, true, true) => Status::WouldDisconnect,
        (Action::Disconnect, false, _) => Status::NotConnected,
    };
    if action == Action::Connect && !dry_run {
        report.notes.push(agent.sign_in_hint().to_string());
    }
    if !diffs.is_empty() {
        report.diff = Some(diffs.join("\n"));
    }
    report
}

/// `<api base>/mcp`: the MCP server lives on the same host as the API the CLI calls.
pub fn mcp_url(api_base: &str) -> Result<String, AutterError> {
    let url = format!("{}/mcp", api_base.trim().trim_end_matches('/'));
    match url::Url::parse(&url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => Ok(url),
        _ => Err(AutterError::Generic(format!(
            "The Autter API address ({api_base}) is not an http(s) URL; check `autter config api_base_url`."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const URL: &str = "https://api.autter.dev/mcp";

    fn env(home: &Path) -> McpEnv {
        McpEnv {
            home: home.to_path_buf(),
            claude_dir: home.join(".claude"),
            claude_json: home.join(".claude.json"),
            vscode_user_dir: home.join("vscode-user"),
            opencode_dir: home.join(".config").join("opencode"),
            on_path: |_| false,
            editor_installed: |_| false,
        }
    }

    fn no_cli(_: &[String]) -> Result<(), String> {
        panic!("no agent CLI may run when none is on PATH")
    }

    fn connect(agent: McpAgent, env: &McpEnv) -> AgentReport {
        apply(agent, Action::Connect, URL, env, false, &mut no_cli)
    }

    fn disconnect(agent: McpAgent, env: &McpEnv) -> AgentReport {
        apply(agent, Action::Disconnect, URL, env, false, &mut no_cli)
    }

    fn read_json(path: &Path) -> Value {
        let text = fs::read_to_string(path).unwrap();
        parse_json(&text).unwrap().to_serde_value().unwrap()
    }

    fn backups_of(path: &Path) -> usize {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{name}.autter-backup-"))
            })
            .count()
    }

    #[test]
    fn agents_parse_by_their_common_names() {
        assert_eq!(McpAgent::parse("Claude-Code"), Some(McpAgent::ClaudeCode));
        assert_eq!(McpAgent::parse("code"), Some(McpAgent::VsCode));
        assert_eq!(McpAgent::parse("gemini-cli"), Some(McpAgent::Gemini));
        assert_eq!(McpAgent::parse("windsurf"), None);
        for agent in McpAgent::ALL {
            assert_eq!(McpAgent::parse(agent.id()), Some(agent));
        }
    }

    #[test]
    fn the_url_is_the_api_base_plus_mcp() {
        assert_eq!(
            mcp_url("https://api.autter.dev").unwrap(),
            "https://api.autter.dev/mcp"
        );
        assert_eq!(
            mcp_url("http://localhost:3000/").unwrap(),
            "http://localhost:3000/mcp"
        );
        assert!(mcp_url("not a url").is_err());
    }

    #[test]
    fn agents_that_are_not_installed_are_left_alone() {
        let home = TempDir::new().unwrap();
        let env = env(home.path());
        for agent in McpAgent::ALL {
            let report = connect(agent, &env);
            assert_eq!(report.status, Status::NotInstalled, "{}", agent.name());
        }
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    }

    #[test]
    fn claude_code_without_its_cli_gets_the_entry_in_claude_json() {
        let home = TempDir::new().unwrap();
        let env = env(home.path());
        let file = home.path().join(".claude.json");
        fs::write(
            &file,
            "{\n  \"numStartups\": 12,\n  \"mcpServers\": {\n    \"other\": { \"type\": \"stdio\", \"command\": \"x\" }\n  },\n  \"projects\": {}\n}\n",
        )
        .unwrap();
        let report = connect(McpAgent::ClaudeCode, &env);
        assert_eq!(report.status, Status::Connected);
        assert_eq!(report.backups.len(), 1);
        let value = read_json(&file);
        assert_eq!(
            value["mcpServers"]["autter"],
            json!({ "type": "http", "url": URL })
        );
        assert_eq!(value["mcpServers"]["other"]["command"], "x");
        assert_eq!(value["numStartups"], 12);
        // Key order is kept: numStartups still comes first.
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("{\n  \"numStartups\": 12,"), "{text}");
        // Idempotent: a second run changes nothing and backs up nothing.
        let again = connect(McpAgent::ClaudeCode, &env);
        assert_eq!(again.status, Status::AlreadyConnected);
        assert_eq!(fs::read_to_string(&file).unwrap(), text);
        assert_eq!(backups_of(&file), 1);
        // Disconnect removes only autter.
        let report = disconnect(McpAgent::ClaudeCode, &env);
        assert_eq!(report.status, Status::Disconnected);
        let value = read_json(&file);
        assert!(value["mcpServers"].get("autter").is_none());
        assert_eq!(value["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            disconnect(McpAgent::ClaudeCode, &env).status,
            Status::NotConnected
        );
    }

    #[test]
    fn claude_code_with_its_cli_runs_claude_mcp() {
        let home = TempDir::new().unwrap();
        let mut env = env(home.path());
        env.on_path = |name| name == "claude";
        let mut ran: Vec<String> = Vec::new();
        let mut runner = |argv: &[String]| -> Result<(), String> {
            ran.push(argv.join(" "));
            Ok(())
        };
        let report = apply(
            McpAgent::ClaudeCode,
            Action::Connect,
            URL,
            &env,
            false,
            &mut runner,
        );
        assert_eq!(report.status, Status::Connected);
        assert_eq!(
            ran,
            vec![format!(
                "claude mcp add --transport http --scope user autter {URL}"
            )]
        );
        // An entry that differs is removed first, then added again.
        fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers":{"autter":{"type":"http","url":"https://old.example/mcp"}}}"#,
        )
        .unwrap();
        ran.clear();
        let mut runner = |argv: &[String]| -> Result<(), String> {
            ran.push(argv.join(" "));
            Ok(())
        };
        let report = apply(
            McpAgent::ClaudeCode,
            Action::Connect,
            URL,
            &env,
            false,
            &mut runner,
        );
        assert_eq!(report.status, Status::Connected);
        assert_eq!(report.backups.len(), 1);
        assert_eq!(ran.len(), 2);
        assert_eq!(ran[0], "claude mcp remove autter --scope user");
        // Dry run only says what it would run.
        let report = apply(
            McpAgent::ClaudeCode,
            Action::Disconnect,
            URL,
            &env,
            true,
            &mut no_cli,
        );
        assert_eq!(report.status, Status::WouldDisconnect);
        assert_eq!(
            report.via,
            vec!["would run: claude mcp remove autter --scope user"]
        );
    }

    #[test]
    fn a_failing_agent_cli_falls_back_to_editing_the_file() {
        let home = TempDir::new().unwrap();
        let mut env = env(home.path());
        env.on_path = |name| name == "codex";
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        let mut runner =
            |_: &[String]| -> Result<(), String> { Err("unexpected argument '--url'".into()) };
        let report = apply(
            McpAgent::Codex,
            Action::Connect,
            URL,
            &env,
            false,
            &mut runner,
        );
        assert_eq!(report.status, Status::Connected);
        assert!(report.notes[0].contains("--url"), "{:?}", report.notes);
        let text = fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
        assert_eq!(
            toml_entry_state(&text, SERVER_NAME, URL).unwrap(),
            EntryState::Same
        );
    }

    #[test]
    fn codex_config_toml_keeps_comments_and_other_servers() {
        let home = TempDir::new().unwrap();
        let env = env(home.path());
        let file = home.path().join(".codex").join("config.toml");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let original = "# my settings\nmodel = \"gpt-5\"\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\" # local\n\n[mcp_servers.autter]\nurl = \"https://old.example/mcp\"\nbearer_token_env_var = \"X\"\n\n[profiles.fast]\nmodel = \"gpt-5-mini\"\n";
        fs::write(&file, original).unwrap();
        let report = connect(McpAgent::Codex, &env);
        assert_eq!(report.status, Status::Connected);
        assert_eq!(report.backups.len(), 1);
        let text = fs::read_to_string(&file).unwrap();
        assert!(
            text.starts_with("# my settings\nmodel = \"gpt-5\"\n"),
            "{text}"
        );
        assert!(text.contains("command = \"docs-mcp\" # local"), "{text}");
        assert!(
            text.contains(&format!("[mcp_servers.autter]\nurl = \"{URL}\"\n")),
            "{text}"
        );
        assert!(!text.contains("bearer_token_env_var"), "{text}");
        assert!(text.contains("[profiles.fast]"), "{text}");
        assert_eq!(text.matches("[mcp_servers.autter]").count(), 1);
        assert_eq!(
            connect(McpAgent::Codex, &env).status,
            Status::AlreadyConnected
        );

        let report = disconnect(McpAgent::Codex, &env);
        assert_eq!(report.status, Status::Disconnected);
        let text = fs::read_to_string(&file).unwrap();
        assert!(!text.contains("autter"), "{text}");
        assert!(text.contains("[mcp_servers.docs]"), "{text}");

        // A config without any servers gets the table.
        fs::write(&file, "model = \"gpt-5\"\n").unwrap();
        connect(McpAgent::Codex, &env);
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(
            toml_entry_state(&text, SERVER_NAME, URL).unwrap(),
            EntryState::Same
        );
        assert!(text.starts_with("model = \"gpt-5\"\n"), "{text}");
    }

    #[test]
    fn cursor_gets_a_url_entry_in_its_global_mcp_json() {
        let home = TempDir::new().unwrap();
        let env = env(home.path());
        fs::create_dir_all(home.path().join(".cursor")).unwrap();
        let report = connect(McpAgent::Cursor, &env);
        assert_eq!(report.status, Status::Connected);
        // A new file: nothing to back up.
        assert!(report.backups.is_empty());
        let file = home.path().join(".cursor/mcp.json");
        assert_eq!(
            read_json(&file),
            json!({ "mcpServers": { "autter": { "url": URL } } })
        );
        assert_eq!(
            connect(McpAgent::Cursor, &env).status,
            Status::AlreadyConnected
        );
        assert_eq!(
            disconnect(McpAgent::Cursor, &env).status,
            Status::Disconnected
        );
        assert_eq!(read_json(&file), json!({ "mcpServers": {} }));
    }

    #[test]
    fn vscode_gets_a_servers_entry_in_each_profile_that_has_one() {
        let home = TempDir::new().unwrap();
        let env = env(home.path());
        let user = home.path().join("vscode-user");
        let profile = user.join("profiles").join("abc123");
        fs::create_dir_all(&profile).unwrap();
        // JSONC with a comment and a trailing comma, as VS Code allows.
        fs::write(
            profile.join("mcp.json"),
            "{\n\t// mine\n\t\"servers\": {\n\t\t\"gh\": { \"type\": \"http\", \"url\": \"https://gh.example/mcp\" },\n\t},\n}\n",
        )
        .unwrap();
        let report = connect(McpAgent::VsCode, &env);
        assert_eq!(report.status, Status::Connected, "{:?}", report.notes);
        assert_eq!(report.via.len(), 2);
        assert_eq!(
            read_json(&user.join("mcp.json"))["servers"]["autter"],
            json!({ "type": "http", "url": URL })
        );
        let text = fs::read_to_string(profile.join("mcp.json")).unwrap();
        assert!(text.contains("// mine"), "{text}");
        let value = read_json(&profile.join("mcp.json"));
        assert_eq!(value["servers"]["autter"]["url"], URL);
        assert_eq!(value["servers"]["gh"]["url"], "https://gh.example/mcp");
        // A profile without its own mcp.json is not given one.
        fs::create_dir_all(user.join("profiles").join("empty")).unwrap();
        assert_eq!(
            connect(McpAgent::VsCode, &env).status,
            Status::AlreadyConnected
        );
        assert!(!user.join("profiles/empty/mcp.json").exists());
    }

    #[test]
    fn gemini_gets_an_http_url_entry_next_to_its_hooks() {
        let home = TempDir::new().unwrap();
        let env = env(home.path());
        let file = home.path().join(".gemini").join("settings.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            "{\n  \"hooks\": { \"BeforeTool\": [] },\n  \"theme\": \"Dracula\"\n}\n",
        )
        .unwrap();
        assert_eq!(connect(McpAgent::Gemini, &env).status, Status::Connected);
        let value = read_json(&file);
        assert_eq!(value["mcpServers"]["autter"], json!({ "httpUrl": URL }));
        assert_eq!(value["theme"], "Dracula");
        assert_eq!(value["hooks"], json!({ "BeforeTool": [] }));
    }

    #[test]
    fn opencode_gets_a_remote_entry_and_prefers_an_existing_jsonc() {
        let home = TempDir::new().unwrap();
        let env = env(home.path());
        let dir = home.path().join(".config").join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("opencode.jsonc"),
            "{\n  // theme\n  \"$schema\": \"https://opencode.ai/config.json\"\n}\n",
        )
        .unwrap();
        assert_eq!(connect(McpAgent::OpenCode, &env).status, Status::Connected);
        assert!(!dir.join("opencode.json").exists());
        let value = read_json(&dir.join("opencode.jsonc"));
        assert_eq!(
            value["mcp"]["autter"],
            json!({ "type": "remote", "url": URL, "enabled": true })
        );
        assert_eq!(value["$schema"], "https://opencode.ai/config.json");
        assert!(
            fs::read_to_string(dir.join("opencode.jsonc"))
                .unwrap()
                .contains("// theme")
        );
    }

    #[test]
    fn dry_run_shows_the_change_and_writes_nothing() {
        let home = TempDir::new().unwrap();
        let env = env(home.path());
        let file = home.path().join(".gemini").join("settings.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "{}\n").unwrap();
        let report = apply(
            McpAgent::Gemini,
            Action::Connect,
            URL,
            &env,
            true,
            &mut no_cli,
        );
        assert_eq!(report.status, Status::WouldConnect);
        assert!(report.diff.as_deref().unwrap().contains("httpUrl"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "{}\n");
        assert_eq!(backups_of(&file), 0);
    }

    #[test]
    fn a_file_that_is_not_an_object_is_left_alone() {
        let home = TempDir::new().unwrap();
        let env = env(home.path());
        let file = home.path().join(".cursor").join("mcp.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "{ \"mcpServers\": [] }").unwrap();
        let report = connect(McpAgent::Cursor, &env);
        assert_eq!(report.status, Status::Failed);
        assert_eq!(fs::read_to_string(&file).unwrap(), "{ \"mcpServers\": [] }");
        fs::write(&file, "{ not json").unwrap();
        assert_eq!(connect(McpAgent::Cursor, &env).status, Status::Failed);
    }
}
