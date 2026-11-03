//! `autter mcp connect|disconnect`: register Autter's MCP server with every
//! coding agent on this machine, or take it out again.
//!
//! The server is `<api base>/mcp` (the API this CLI talks to) and signs people
//! in with OAuth, so only its URL is written; each agent asks the person to
//! sign in the first time it uses it. See `crate::mdm::mcp_servers` for each
//! agent's file and format.
use crate::commands::arg_parser::{self, ScanMode};
use crate::config::Config;
use crate::error::AutterError;
use crate::mdm::mcp_servers::{self, Action, AgentReport, McpAgent, McpEnv, Status};
use serde_json::json;
use std::path::Path;

#[derive(Debug, PartialEq)]
struct Options {
    action: Action,
    agents: Vec<McpAgent>,
    dry_run: bool,
}

fn agent_names() -> String {
    McpAgent::ALL
        .iter()
        .map(|agent| agent.id())
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut action = None;
    let mut agents: Vec<McpAgent> = Vec::new();
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--dry-run" => dry_run = true,
            "--agent" => {
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.starts_with("--"))
                    .ok_or("--agent requires a value")?;
                for name in value.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    let agent = McpAgent::parse(name).ok_or_else(|| {
                        format!("unknown agent {name}; --agent takes: {}", agent_names())
                    })?;
                    if !agents.contains(&agent) {
                        agents.push(agent);
                    }
                }
                index += 1;
            }
            "connect" if action.is_none() => action = Some(Action::Connect),
            "disconnect" if action.is_none() => action = Some(Action::Disconnect),
            other if other.starts_with('-') => {
                return Err(format!("unknown option for mcp: {other}"));
            }
            other if action.is_none() => {
                return Err(format!("unknown mcp subcommand: {other}"));
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
        index += 1;
    }
    let action = action.ok_or("mcp needs a subcommand: connect or disconnect")?;
    if agents.is_empty() {
        agents = McpAgent::ALL.to_vec();
    }
    Ok(Options {
        action,
        agents,
        dry_run,
    })
}

/// `path` with the home directory shown as `~`.
fn tilde(path: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    match path.strip_prefix(home.as_ref()) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

fn render(url: &str, options: &Options, reports: &[AgentReport], home: &Path) -> String {
    let width = reports
        .iter()
        .map(|report| report.name.chars().count())
        .max()
        .unwrap_or(0);
    let indent = " ".repeat(width + 4);
    let mut out = format!("Autter MCP server: {url}\n\n");
    for report in reports {
        let mut line = format!("  {:<width$}  {}", report.name, report.status.label());
        let via: Vec<String> = report.via.iter().map(|v| tilde(v, home)).collect();
        if !via.is_empty() && report.status != Status::NotInstalled {
            line.push_str(&format!("  ({})", via.join("; ")));
        }
        out.push_str(&line);
        out.push('\n');
        for backup in &report.backups {
            out.push_str(&format!(
                "{indent}backup: {}\n",
                tilde(&backup.display().to_string(), home)
            ));
        }
        for note in &report.notes {
            out.push_str(&format!("{indent}{}\n", tilde(note, home)));
        }
        if let Some(diff) = &report.diff {
            for diff_line in diff.lines() {
                out.push_str(&format!("{indent}{}\n", tilde(diff_line, home)));
            }
        }
    }
    let touched = reports
        .iter()
        .any(|report| report.status != Status::NotInstalled);
    if !touched {
        out.push_str(&format!(
            "\nNo supported agent found. `autter mcp` configures: {}.\n",
            McpAgent::ALL
                .iter()
                .map(|agent| agent.name())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else if options.dry_run {
        out.push_str("\nDry run: nothing was changed.\n");
    } else if options.action == Action::Connect {
        out.push_str(
            "\nThe server signs you in with your Autter account (OAuth); autter writes no token.\n",
        );
    }
    out
}

fn run(options: &Options) -> Result<bool, AutterError> {
    let url = mcp_servers::mcp_url(Config::get().api_base_url())?;
    let env = McpEnv::system();
    let mut runner = mcp_servers::run_command;
    let reports: Vec<AgentReport> = options
        .agents
        .iter()
        .map(|agent| {
            mcp_servers::apply(
                *agent,
                options.action,
                &url,
                &env,
                options.dry_run,
                &mut runner,
            )
        })
        .collect();
    if arg_parser::json() {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "url": url,
                "dry_run": options.dry_run,
                "agents": reports,
            }))?
        );
    } else {
        print!("{}", render(&url, options, &reports, &env.home));
    }
    Ok(reports.iter().all(|report| report.status != Status::Failed))
}

pub fn handle(args: &[String]) {
    let parsed = arg_parser::pre_parse(args, ScanMode::Full, false).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        std::process::exit(2);
    });
    arg_parser::merge_global_flags(&parsed.flags);
    if parsed.flags.help {
        arg_parser::print_command_help("mcp");
        return;
    }
    let options = parse_options(&parsed.rest).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        arg_parser::print_command_help("mcp");
        std::process::exit(2);
    });
    match run(&options) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            match error {
                AutterError::Generic(message) => eprintln!("error: {message}"),
                other => eprintln!("error: {other}"),
            }
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn connect_defaults_to_every_agent_and_takes_filters() {
        let all = parse_options(&args(&["connect"])).unwrap();
        assert_eq!(all.action, Action::Connect);
        assert_eq!(all.agents, McpAgent::ALL.to_vec());
        assert!(!all.dry_run);
        let some = parse_options(&args(&[
            "disconnect",
            "--agent",
            "claude",
            "--agent",
            "cursor,vscode",
            "--agent",
            "claude-code",
            "--dry-run",
        ]))
        .unwrap();
        assert_eq!(some.action, Action::Disconnect);
        assert_eq!(
            some.agents,
            vec![McpAgent::ClaudeCode, McpAgent::Cursor, McpAgent::VsCode]
        );
        assert!(some.dry_run);
    }

    #[test]
    fn bad_usage_is_refused() {
        assert!(parse_options(&args(&[])).is_err());
        assert!(parse_options(&args(&["--dry-run"])).is_err());
        assert!(parse_options(&args(&["status"])).is_err());
        assert!(parse_options(&args(&["connect", "--agent"])).is_err());
        assert!(parse_options(&args(&["connect", "--agent", "windsurf"])).is_err());
        assert!(parse_options(&args(&["connect", "--token", "x"])).is_err());
        assert!(parse_options(&args(&["connect", "disconnect"])).is_err());
    }

    #[test]
    fn each_agent_gets_a_result_line() {
        let home = PathBuf::from("/home/ana");
        let options = parse_options(&args(&["connect"])).unwrap();
        let reports = vec![
            AgentReport {
                agent: "claude",
                name: "Claude Code",
                status: Status::Connected,
                via: vec![
                    "ran: claude mcp add --transport http --scope user autter https://api.autter.dev/mcp"
                        .into(),
                ],
                backups: vec![PathBuf::from("/home/ana/.claude.json.autter-backup-20261103-090000")],
                notes: vec!["sign in: run /mcp in Claude Code and choose autter".into()],
                diff: None,
            },
            AgentReport {
                agent: "cursor",
                name: "Cursor",
                status: Status::AlreadyConnected,
                via: vec!["/home/ana/.cursor/mcp.json".into()],
                backups: vec![],
                notes: vec![],
                diff: None,
            },
            AgentReport {
                agent: "gemini",
                name: "Gemini CLI",
                status: Status::NotInstalled,
                via: vec![],
                backups: vec![],
                notes: vec![],
                diff: None,
            },
        ];
        assert_eq!(
            render("https://api.autter.dev/mcp", &options, &reports, &home),
            "Autter MCP server: https://api.autter.dev/mcp\n\n\
             \x20 Claude Code  connected  (ran: claude mcp add --transport http --scope user autter https://api.autter.dev/mcp)\n\
             \x20              backup: ~/.claude.json.autter-backup-20261103-090000\n\
             \x20              sign in: run /mcp in Claude Code and choose autter\n\
             \x20 Cursor       already connected  (~/.cursor/mcp.json)\n\
             \x20 Gemini CLI   not installed\n\
             \nThe server signs you in with your Autter account (OAuth); autter writes no token.\n"
        );
    }
}
