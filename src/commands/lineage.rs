//! `autter lineage <issue-id>`: from a Runtime issue to the release, commit,
//! PR, owner and AI authorship behind it.
//!
//! Reads `GET /api/cli/orgs/<org>/runtime/issues/<id>/lineage[?repo=<name>]`
//! with the sign-in and organization choice of `autter ask` and prints the
//! chain top to bottom: the issue, the release it first appeared in and the
//! commit it runs, the suspect PR, the owner, the suspect commits, who wrote
//! each failing frame (an AI agent, a person, or not known), and why the error
//! was grouped into this issue. `--json` prints the response.
//!
//! The server sends no prompt text and no transcript hash; for a frame an
//! agent wrote with a stored transcript, the command prints the
//! `autter show-prompt <session>` to run. Nobody is shown by email address.
//!
//! `autter lineage <file>:<line>` is `autter blame --why <file>:<line>`: the
//! dispatcher sends it there (see [`blame_why_args`]).
use crate::api::client::ApiContext;
use crate::commands::arg_parser::{self, ScanMode};
use crate::commands::ask::{component, org_api_context};
use crate::error::AutterError;
use serde_json::Value;

/// The lineage route reads suspects, owners and authorship notes (GitHub
/// reads on a first visit), so it can take longer than a plain read.
const TIMEOUT_SECS: u64 = 120;

const NOT_SIGNED_IN: &str = "Autter did not accept your sign-in for this organization (HTTP 401). Sign in with `autter login` first, or choose an organization with --org <slug>, or sign in again.";
const ISSUE_NOT_FOUND: &str = "Issue not found in this org (or repository).";

#[derive(Debug, Default, PartialEq)]
struct Options {
    issue: Option<String>,
    repository: Option<String>,
    organization: Option<String>,
}

/// A file and line (`src/orders.ts:42`), as opposed to an issue id: it has a
/// colon and is not a URL. Issue ids (`ri_…`) never contain a colon.
fn is_file_line(arg: &str) -> bool {
    arg.contains(':') && !arg.contains("://")
}

/// When the target is `<file>:<line>`, the arguments for `autter blame` that
/// explain that line (`--why <file>:<line>` and the rest as given), so the
/// line is explained by blame's own code path. `None` for an issue id.
pub fn blame_why_args(args: &[String]) -> Option<Vec<String>> {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            index += 1;
            break;
        }
        if matches!(arg, "--repo" | "--org" | "-C") {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }
    let target = args.get(index)?;
    if !is_file_line(target) {
        return None;
    }
    let mut forwarded = vec!["--why".to_owned(), target.clone()];
    forwarded.extend(
        args.iter()
            .enumerate()
            .filter(|(position, arg)| *position != index && arg.as_str() != "--")
            .map(|(_, arg)| arg.clone()),
    );
    Some(forwarded)
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            for word in &args[index + 1..] {
                set_issue(&mut options, word)?;
            }
            break;
        }
        if !arg.starts_with('-') {
            set_issue(&mut options, arg)?;
            index += 1;
            continue;
        }
        if !matches!(arg, "--repo" | "--org") {
            return Err(format!("unknown option for lineage: {arg}"));
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.starts_with("--") && !value.trim().is_empty())
            .ok_or_else(|| format!("{arg} requires a value"))?;
        match arg {
            "--repo" => options.repository = Some(value.trim().to_owned()),
            _ => options.organization = Some(value.trim().to_owned()),
        }
        index += 2;
    }
    Ok(options)
}

fn set_issue(options: &mut Options, word: &str) -> Result<(), String> {
    let word = word.trim();
    if word.contains("://") {
        return Err(
            "lineage takes the issue id (ri_...), not a URL: the id is the last part of the issue page's address"
                .into(),
        );
    }
    if word.is_empty() {
        return Ok(());
    }
    if options.issue.is_some() {
        return Err("lineage takes one issue id".into());
    }
    options.issue = Some(word.to_owned());
    Ok(())
}

/// The route for an issue, with the optional repository name.
fn lineage_path(slug: &str, issue: &str, repository: Option<&str>) -> String {
    let mut path = format!(
        "/api/cli/orgs/{}/runtime/issues/{}/lineage",
        component(slug),
        component(issue)
    );
    if let Some(repository) = repository {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("repo", repository)
            .finish();
        path.push('?');
        path.push_str(&query);
    }
    path
}

/// The lineage JSON of a 200, else the error the command prints for it.
fn response_value(code: u16, body: &str) -> Result<Value, AutterError> {
    let value: Option<Value> = serde_json::from_str(body).ok();
    if code == 200
        && let Some(value) = value
    {
        return Ok(value);
    }
    if code == 401 {
        return Err(AutterError::Generic(NOT_SIGNED_IN.into()));
    }
    let Some(value) = value else {
        return Err(AutterError::Generic(format!(
            "Lineage API returned HTTP {code}. Update the API deployment if this command is unavailable."
        )));
    };
    let message = value["error"].as_str().map(str::to_owned);
    Err(AutterError::Generic(match (code, message) {
        (404, Some(message)) if message == "Issue not found" => ISSUE_NOT_FOUND.to_owned(),
        (400, Some(message)) if message.starts_with("Unknown repository") => format!(
            "{}. Pass the repository's name as Autter shows it with --repo, or leave --repo out.",
            message.trim_end_matches('.')
        ),
        (_, message) => format!(
            "HTTP {code}: {}",
            message.as_deref().unwrap_or("Lineage request failed")
        ),
    }))
}

fn fetch(context: &ApiContext, slug: &str, options: &Options) -> Result<Value, AutterError> {
    let issue = options.issue.as_deref().unwrap_or_default();
    let response = context.get(&lineage_path(slug, issue, options.repository.as_deref()))?;
    let body = response
        .as_str()
        .map_err(|error| AutterError::Generic(error.to_string()))?;
    response_value(response.status_code, body)
}

fn run(options: Options) -> Result<(), AutterError> {
    let (context, slug) = org_api_context(options.organization.as_deref(), TIMEOUT_SECS)?;
    let lineage = fetch(&context, &slug, &options)?;
    if arg_parser::json() {
        println!("{}", serde_json::to_string_pretty(&lineage)?);
    } else {
        print!("{}", render(&lineage));
    }
    Ok(())
}

pub fn handle(args: &[String]) {
    let parsed = arg_parser::pre_parse(args, ScanMode::Full, true).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        std::process::exit(2);
    });
    arg_parser::merge_global_flags(&parsed.flags);
    if parsed.flags.help {
        arg_parser::print_command_help("lineage");
        return;
    }
    if let Some(directory) = parsed.flags.change_dir
        && let Err(error) = std::env::set_current_dir(directory)
    {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
    let options = parse_options(&parsed.rest).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        std::process::exit(2);
    });
    if options.issue.is_none() {
        arg_parser::print_command_help("lineage");
        std::process::exit(2);
    }
    if let Err(error) = run(options) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Text without terminal control characters, on one line.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a word is an email address (`jane@acme.com`, `<jane@acme.com>`).
/// Only applied to text about people: a frame such as `handler@orders.ts`
/// has the same shape.
fn is_email(word: &str) -> bool {
    let word = word.trim_matches(|c: char| "<>()[]\"',;".contains(c));
    let Some((local, domain)) = word.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain
            .split_once('.')
            .is_some_and(|(host, tld)| !host.is_empty() && !tld.is_empty())
}

/// A string field, cleaned; `None` when absent, null or blank.
fn text(value: &Value) -> Option<String> {
    value.as_str().map(plain).filter(|text| !text.is_empty())
}

/// A field that names or describes a person (names, owner reasons), with any
/// email address hidden. The server already hides them; this keeps any it
/// missed off the screen.
fn people_text(value: &Value) -> Option<String> {
    text(value).map(|text| {
        text.split(' ')
            .map(|word| {
                if is_email(word) {
                    "(email hidden)"
                } else {
                    word
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    })
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// `2026-11-14T09:12:30.000Z` as `2026-11-14 09:12 UTC`.
fn when(value: &Value) -> Option<String> {
    let raw = value.as_str()?;
    Some(
        chrono::DateTime::parse_from_rfc3339(raw)
            .map(|time| {
                time.with_timezone(&chrono::Utc)
                    .format("%Y-%m-%d %H:%M UTC")
                    .to_string()
            })
            .unwrap_or_else(|_| plain(raw)),
    )
}

/// An agent's display name from its tool id in the authorship notes.
fn agent_name(tool: &str) -> String {
    let name = match tool.to_ascii_lowercase().as_str() {
        "claude" | "claude-code" | "claude_code" => "Claude",
        "codex" => "Codex",
        "cursor" => "Cursor",
        "gemini" => "Gemini",
        "github-copilot" | "copilot" => "GitHub Copilot",
        "windsurf" => "Windsurf",
        "amp" => "Amp",
        "opencode" => "OpenCode",
        "continue-cli" | "continue" => "Continue",
        "droid" => "Droid",
        "kiro" => "Kiro",
        "pi" => "Pi",
        _ => return plain(tool),
    };
    name.to_owned()
}

fn list_text(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The response's notes, each printed once: a part that could not be read
/// prints its matching note, and the rest are listed at the end.
struct Notes {
    notes: Vec<String>,
    used: Vec<bool>,
}

impl Notes {
    fn new(value: &Value) -> Self {
        let notes: Vec<String> = value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text)
            .collect();
        let used = vec![false; notes.len()];
        Self { notes, used }
    }

    /// The first unused note that mentions one of `keywords` (lowercase).
    fn take(&mut self, keywords: &[&str]) -> Option<String> {
        let position = self.notes.iter().enumerate().position(|(index, note)| {
            let lower = note.to_lowercase();
            !self.used[index] && keywords.iter().any(|keyword| lower.contains(keyword))
        })?;
        self.used[position] = true;
        Some(self.notes[position].clone())
    }

    fn rest(&self) -> Vec<&String> {
        self.notes
            .iter()
            .zip(&self.used)
            .filter(|(_, used)| !**used)
            .map(|(note, _)| note)
            .collect()
    }
}

/// Notes inside a part (`owner.notes`, `suspects.notes`, ...).
fn part_notes(out: &mut Vec<String>, value: &Value, indent: &str) {
    for note in value.as_array().into_iter().flatten().filter_map(text) {
        out.push(format!("{indent}Note: {note}"));
    }
}

/// One line for a part that is null, with the note that says why.
fn unread(out: &mut Vec<String>, what: &str, notes: &mut Notes, keywords: &[&str]) {
    out.push(format!("  {what} could not be read."));
    if let Some(note) = notes.take(keywords) {
        out.push(format!("  {note}"));
    }
}

/// The lineage as plain text, top to bottom.
fn render(lineage: &Value) -> String {
    let mut notes = Notes::new(&lineage["notes"]);
    let mut out: Vec<String> = Vec::new();
    // A missing repository makes every repository-based part null, with one note.
    let no_repo = ["no repository"];

    render_issue(&mut out, &lineage["issue"]);

    out.push(String::new());
    out.push("Release".into());
    if lineage["release"].is_null() {
        unread(
            &mut out,
            "The release",
            &mut notes,
            &["release this error first appeared in", "release"],
        );
    } else {
        render_release(&mut out, &lineage["release"], &mut notes);
    }

    out.push(String::new());
    out.push("Pull request".into());
    match (&lineage["pr"], &lineage["suspects"]) {
        (pr, _) if !pr.is_null() => {
            let number = pr["number"].as_u64().map(|n| format!("#{n}"));
            let title = text(&pr["title"]);
            let line = [number, title].into_iter().flatten().collect::<Vec<_>>();
            let suspect = text(&pr["suspectId"])
                .map(|id| format!(" (from suspect commit {id})"))
                .unwrap_or_default();
            out.push(format!("  {}{suspect}", line.join(" ")));
            if let Some(url) = text(&pr["url"]) {
                out.push(format!("  {url}"));
            }
        }
        (_, suspects) if suspects.is_null() => out.push(
            "  The pull request could not be read: it comes from the suspect commits, which could not be read."
                .into(),
        ),
        _ => out.push("  No suspect commit in this repository has a pull request.".into()),
    }

    out.push(String::new());
    out.push("Owner".into());
    if lineage["owner"].is_null() {
        unread(&mut out, "The owner", &mut notes, &["owners", no_repo[0]]);
    } else {
        render_owner(&mut out, &lineage["owner"]);
    }

    out.push(String::new());
    out.push("Suspect commits".into());
    if lineage["suspects"].is_null() {
        unread(
            &mut out,
            "The suspect commits",
            &mut notes,
            &["suspect commits", no_repo[0]],
        );
    } else {
        render_suspects(&mut out, &lineage["suspects"]);
    }

    out.push(String::new());
    out.push("Who wrote the failing code".into());
    if lineage["frameAuthorship"].is_null() {
        unread(
            &mut out,
            "Who wrote the failing frames",
            &mut notes,
            &["frame authorship", no_repo[0]],
        );
    } else {
        render_frames(&mut out, &lineage["frameAuthorship"]);
    }

    out.push(String::new());
    out.push("Why this error was grouped into this issue".into());
    if lineage["whyLinked"].is_null() {
        unread(
            &mut out,
            "Why it was grouped",
            &mut notes,
            &["why this occurrence joined", no_repo[0]],
        );
    } else {
        let why = &lineage["whyLinked"];
        out.push(format!(
            "  {}",
            text(&why["reason"]).unwrap_or_else(|| "No reason was given.".into())
        ));
        if let Some(reason) = text(&why["levelReason"]) {
            out.push(format!("  {reason}"));
        }
    }

    let rest = notes.rest();
    if !rest.is_empty() {
        out.push(String::new());
        out.push("Notes".into());
        for note in rest {
            out.push(format!("  - {note}"));
        }
    }
    let mut rendered = out.join("\n");
    rendered.push('\n');
    rendered
}

fn render_issue(out: &mut Vec<String>, issue: &Value) {
    let id = text(&issue["id"]).unwrap_or_default();
    let title = text(&issue["title"]).unwrap_or_else(|| "(no title)".into());
    out.push(format!("Issue {id}: {title}"));
    let mut facts = Vec::new();
    if let Some(service) = text(&issue["service"]) {
        facts.push(format!("service {service}"));
    }
    if let Some(environment) = text(&issue["environment"]) {
        facts.push(format!("environment {environment}"));
    }
    if let Some(status) = text(&issue["status"]) {
        facts.push(format!("status {status}"));
    }
    if let Some(first) = when(&issue["firstSeenAt"]) {
        facts.push(format!("first seen {first}"));
    }
    if !facts.is_empty() {
        out.push(format!("  {}", facts.join(" · ")));
    }
    if let Some(repository) = text(&issue["repository"]) {
        out.push(format!("  Repository: {repository}"));
    }
}

fn render_release(out: &mut Vec<String>, release: &Value, notes: &mut Notes) {
    let label = text(&release["label"]);
    match text(&release["name"]) {
        Some(name) => out.push(format!("  First seen in release {name}")),
        None => out.push("  The app sent no release name for this error.".into()),
    }
    match text(&release["commit"]).filter(|_| release["resolved"] != false) {
        Some(commit) => {
            let from = label
                .map(|label| format!(" (from {label})"))
                .unwrap_or_default();
            out.push(format!("  Commit {}{from}", short_sha(&commit)));
            if let Some(url) = text(&release["commitUrl"]) {
                out.push(format!("  {url}"));
            }
        }
        None => {
            out.push("  Commit: not known, so code was read at the default branch.".into());
            // The server's note says the same about this release; print it once.
            notes.take(&["no commit is known"]);
        }
    }
    let previous = &release["previous"];
    if !previous.is_null() {
        let name = text(&previous["name"]);
        let commit = text(&previous["commit"]).map(|sha| short_sha(&sha).to_owned());
        let line = match (name, commit) {
            (Some(name), Some(commit)) => format!("release {name}, commit {commit}"),
            (Some(name), None) => format!("release {name}, commit not known"),
            (None, Some(commit)) => format!("commit {commit}"),
            (None, None) => text(&previous["label"]).unwrap_or_else(|| "not known".into()),
        };
        out.push(format!("  Before it: {line}"));
    }
}

/// A person or team by name, GitHub login or team, never by email.
fn person(name: &Value, login: &Value, team: &Value) -> String {
    let name = people_text(name);
    let login = people_text(login).map(|login| format!("@{}", login.trim_start_matches('@')));
    let team = people_text(team);
    let mut shown = match (name, login) {
        (Some(name), Some(login)) => format!("{name} ({login})"),
        (Some(name), None) => name,
        (None, Some(login)) => login,
        (None, None) => String::new(),
    };
    if let Some(team) = team {
        if shown.is_empty() {
            shown = format!("team {team}");
        } else {
            shown.push_str(&format!(", team {team}"));
        }
    }
    if shown.is_empty() {
        "someone (no name recorded)".into()
    } else {
        shown
    }
}

fn render_owner(out: &mut Vec<String>, owner: &Value) {
    let assigned = &owner["assigned"];
    let suggestions: Vec<&Value> = owner["suggestions"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    if !assigned.is_null() {
        let via = people_text(&assigned["via"])
            .map(|via| format!(", via {via}"))
            .unwrap_or_default();
        out.push(format!(
            "  Assigned: {}{via}",
            person(&assigned["name"], &Value::Null, &assigned["team"])
        ));
    } else if suggestions.is_empty() {
        out.push("  No owner is assigned and none is suggested.".into());
    } else {
        out.push("  No owner is assigned. Suggested:".into());
    }
    if !assigned.is_null() && !suggestions.is_empty() {
        out.push("  Also suggested:".into());
    }
    for suggestion in suggestions {
        let sources: Vec<String> = suggestion["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text)
            .collect();
        let sources = if sources.is_empty() {
            String::new()
        } else {
            format!(" (from {})", sources.join(", "))
        };
        out.push(format!(
            "    - {}{sources}",
            person(
                &suggestion["name"],
                &suggestion["login"],
                &suggestion["team"]
            )
        ));
        for via in suggestion["via"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(people_text)
        {
            out.push(format!("      {via}"));
        }
    }
    // Owner notes can quote a CODEOWNERS line, so they are text about people.
    for note in owner["notes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(people_text)
    {
        out.push(format!("  Note: {note}"));
    }
}

fn render_suspects(out: &mut Vec<String>, suspects: &Value) {
    let items: Vec<&Value> = suspects["items"].as_array().into_iter().flatten().collect();
    if items.is_empty() {
        out.push("  No suspect commits were found.".into());
    }
    for item in items {
        let id = text(&item["id"]).unwrap_or_default();
        let sha = text(&item["sha"]).unwrap_or_default();
        let headline = text(&item["headline"]).unwrap_or_else(|| "(no message)".into());
        out.push(format!("  {id} {} {headline}", short_sha(&sha)));
        let agents: Vec<String> = item["agents"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text)
            .map(|agent| agent_name(&agent))
            .collect();
        let mut facts = Vec::new();
        if item["aiOnly"] == true {
            let who = if agents.is_empty() {
                "an AI agent".to_owned()
            } else {
                list_text(&agents)
            };
            facts.push(format!("written by {who}, no human author"));
        } else {
            let author = &item["author"];
            let mut by = format!(
                "by {}",
                person(&author["name"], &author["login"], &Value::Null)
            );
            if !agents.is_empty() {
                by.push_str(&format!(" with {}", list_text(&agents)));
            }
            facts.push(by);
        }
        if let Some(number) = item["pr"]["number"].as_u64() {
            facts.push(format!("PR #{number}"));
        }
        match item["inWindow"].as_bool() {
            Some(true) => facts.push("in the release window".into()),
            Some(false) => facts.push("outside the release window".into()),
            None => {}
        }
        if let Some(repository) = text(&item["repository"]) {
            facts.push(format!("in {repository}"));
        }
        if let Some(date) = when(&item["committedAt"]) {
            facts.push(date);
        }
        out.push(format!("      {}", facts.join(" · ")));
        if let Some(reason) = text(&item["reason"]) {
            out.push(format!("      Why: {reason}"));
        }
    }
    if suspects["complete"] == false {
        out.push("  The list may be incomplete.".into());
    }
    part_notes(out, &suspects["notes"], "  ");
}

/// Who wrote a failing frame's line.
fn frame_owner(frame: &Value) -> String {
    let owner = &frame["owner"];
    if owner.is_null() {
        return if frame["coverage"] == "none" {
            "Not known: no authorship notes".into()
        } else {
            "Not known".into()
        };
    }
    let commit = text(&owner["commit"])
        .map(|sha| format!(" in commit {}", short_sha(&sha)))
        .unwrap_or_default();
    if owner["class"] == "human" {
        let name = people_text(&owner["humanAuthor"])
            .unwrap_or_else(|| "A person (name not recorded)".into());
        return format!("{name}{commit}");
    }
    let agent = text(&owner["tool"])
        .map(|tool| agent_name(&tool))
        .unwrap_or_else(|| "An AI agent".into());
    let model = text(&owner["model"])
        .map(|model| format!(" (model {model})"))
        .unwrap_or_default();
    let edited = if owner["humanEdited"] == true {
        ", edited by a person since"
    } else {
        ""
    };
    format!("{agent}{model}{commit}{edited}")
}

fn render_frames(out: &mut Vec<String>, authorship: &Value) {
    let frames: Vec<&Value> = authorship["frames"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    if frames.is_empty() {
        out.push("  No failing frame in this repository's code could be read.".into());
    }
    for frame in frames {
        let path = text(&frame["path"]).unwrap_or_else(|| "(unknown file)".into());
        let line = frame["line"]
            .as_u64()
            .map(|n| format!(":{n}"))
            .unwrap_or_default();
        let function = text(&frame["function"])
            .map(|name| format!(" in {name}"))
            .unwrap_or_default();
        out.push(format!("  {path}{line}{function}"));
        out.push(format!("    {}", frame_owner(frame)));
        let owner = &frame["owner"];
        if owner["class"] != "human"
            && let Some(name) = people_text(&owner["humanAuthor"])
        {
            out.push(format!("    Run by {name}"));
        }
        let share = &frame["functionShare"];
        if let (Some(ai), Some(lines)) = (share["aiLines"].as_u64(), share["lines"].as_u64()) {
            let (start, end) = (share["start"].as_u64(), share["end"].as_u64());
            let range = match (start, end) {
                (Some(start), Some(end)) => format!("lines {start}-{end}"),
                _ => "its lines".into(),
            };
            let span = match (share["span"].as_str(), text(&share["name"])) {
                (Some("function"), Some(name)) => format!("Function {name} ({range})"),
                (Some("function"), None) => format!("The function ({range})"),
                _ => format!("Around the line ({range})"),
            };
            out.push(format!("    {span}: {ai} of {lines} lines written by AI"));
        }
        if frame["lineCoordinates"] == "best_effort" {
            out.push("    Line numbers are approximate.".into());
        }
        // A note that only repeats "not known" adds nothing to the owner line.
        if let Some(note) = text(&frame["note"])
            .filter(|note| !(owner.is_null() && note.to_lowercase().starts_with("not known")))
        {
            out.push(format!("    {note}"));
        }
        if owner["transcriptAvailable"] == true
            && let Some(session) = text(&owner["sessionId"])
        {
            out.push(format!("    Prompt: autter show-prompt {session}"));
        }
    }
    let coverage = authorship["coverage"].as_str();
    if coverage == Some("partial") {
        out.push("  Authorship notes cover only part of this code's history.".into());
    }
    part_notes(out, &authorship["notes"], "  ");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn full_response() -> Value {
        json!({
            "version": 1,
            "issue": {
                "id": "ri_0123456789abcdef0123456789",
                "title": "TypeError: Cannot read properties of undefined (reading 'id')",
                "service": "checkout-api",
                "environment": "production",
                "status": "open",
                "release": "v2.4.1",
                "firstSeenAt": "2026-11-14T09:12:30.000Z",
                "repository": "acme/shop"
            },
            "release": {
                "name": "v2.4.1",
                "commit": "3f9a2c1d4e5f60718293a4b5c6d7e8f901234567",
                "commitSource": "release",
                "resolved": true,
                "label": "GitHub release v2.4.1",
                "commitUrl": "https://github.com/acme/shop/commit/3f9a2c1d4e5f60718293a4b5c6d7e8f901234567",
                "previous": { "name": "v2.4.0", "commit": "1a2b3c4d5e6f", "label": "GitHub release v2.4.0" },
                "environment": "production"
            },
            "pr": { "number": 482, "title": "Cache order lookups", "url": "https://github.com/acme/shop/pull/482", "suspectId": "S1" },
            "owner": {
                "assigned": { "name": "Jane Doe", "team": "payments", "via": "CODEOWNERS: src/orders/ @acme/payments" },
                "suggestions": [
                    { "name": "Sam Lee", "team": null, "login": "samlee", "sources": ["blame"], "via": ["wrote 12 of the failing function's lines"] }
                ],
                "notes": []
            },
            "suspects": {
                "complete": true,
                "notes": [],
                "items": [
                    {
                        "id": "S1", "sha": "3f9a2c1d4e5f60718293a4b5c6d7e8f901234567",
                        "url": "https://github.com/acme/shop/commit/3f9a2c1d", "headline": "Cache order lookups",
                        "committedAt": "2026-11-13T18:00:00.000Z",
                        "author": { "login": "janedoe", "name": "Jane Doe" },
                        "aiOnly": false, "agents": ["Claude"],
                        "pr": { "number": 482, "title": "Cache order lookups", "url": "https://github.com/acme/shop/pull/482" },
                        "tier": "line", "inWindow": true, "reason": "Changed the failing line orders.ts:42.", "repository": null
                    },
                    {
                        "id": "S2", "sha": "9e8d7c6b5a4f",
                        "url": null, "headline": "Refactor order handler",
                        "committedAt": null, "author": null,
                        "aiOnly": true, "agents": ["Claude"],
                        "pr": null, "tier": "function", "inWindow": true,
                        "reason": "Changed the failing function handler.", "repository": null
                    }
                ]
            },
            "frameAuthorship": {
                "ref": { "sha": "3f9a2c1d4e5f60718293a4b5c6d7e8f901234567", "label": "GitHub release v2.4.1", "source": "release" },
                "coverage": "full",
                "notes": [],
                "frames": [
                    {
                        "path": "src/orders.ts", "line": 42, "function": "handler",
                        "coverage": "full", "lineCoordinates": "exact",
                        "owner": { "class": "ai", "tool": "claude", "model": "claude-opus-4-5", "sessionId": "s_3f9a2c1d4e5f60::t_0a1b2c3d4e5f60",
                                   "commit": "3f9a2c1d4e5f60718293a4b5c6d7e8f901234567", "humanEdited": true, "humanAuthor": "Jane Doe", "transcriptAvailable": true },
                        "functionShare": { "span": "function", "name": "handler", "start": 30, "end": 60, "aiLines": 25, "lines": 31, "models": { "claude:claude-opus-4-5": 25 } },
                        "note": null
                    },
                    {
                        "path": "src/db.ts", "line": 10, "function": "query",
                        "coverage": "full", "lineCoordinates": "best_effort",
                        "owner": { "class": "human", "tool": null, "model": null, "sessionId": null,
                                   "commit": "1a2b3c4d5e6f", "humanEdited": false, "humanAuthor": "Sam Lee", "transcriptAvailable": false },
                        "functionShare": null, "note": null
                    },
                    {
                        "path": "src/util.ts", "line": 5, "function": null,
                        "coverage": "none", "lineCoordinates": "exact",
                        "owner": null, "functionShare": null,
                        "note": "Not known: no Autter CLI authorship notes cover this file's commits."
                    }
                ]
            },
            "whyLinked": {
                "level": "occurrence", "scheme": "frames-v3", "via": "key",
                "reason": "Grouped by its top frames: handler@orders.ts, query@db.ts.",
                "occurrence": { "id": "occ_1", "occurredAt": "2026-11-15T08:00:00.000Z" }
            },
            "notes": ["Owners were computed from CODEOWNERS at the release commit."]
        })
    }

    #[test]
    fn an_issue_id_is_fetched_and_a_file_line_goes_to_blame() {
        assert_eq!(blame_why_args(&args(&["ri_abc"])), None);
        assert_eq!(blame_why_args(&args(&["--repo", "shop", "ri_abc"])), None);
        assert_eq!(
            blame_why_args(&args(&["src/orders.ts:42"])),
            Some(args(&["--why", "src/orders.ts:42"]))
        );
        assert_eq!(
            blame_why_args(&args(&["--json", "src/orders.ts:42", "--open"])),
            Some(args(&["--why", "src/orders.ts:42", "--json", "--open"]))
        );
        // A URL has a colon but is not a file and line.
        assert_eq!(
            blame_why_args(&args(&[
                "https://app.autter.dev/acme/runtime/errors/ri_abc"
            ])),
            None
        );
        assert_eq!(blame_why_args(&args(&[])), None);
    }

    #[test]
    fn options_take_one_issue_id_and_reject_urls() {
        let options = parse_options(&args(&["ri_abc", "--repo", "shop", "--org", "acme"])).unwrap();
        assert_eq!(
            options,
            Options {
                issue: Some("ri_abc".into()),
                repository: Some("shop".into()),
                organization: Some("acme".into()),
            }
        );
        let error =
            parse_options(&args(&["https://app.autter.dev/x/runtime/errors/ri_abc"])).unwrap_err();
        assert!(error.contains("issue id"), "{error}");
        assert!(parse_options(&args(&["ri_a", "ri_b"])).is_err());
        assert!(parse_options(&args(&["ri_a", "--repo"])).is_err());
        assert!(parse_options(&args(&["ri_a", "--repo", "--org", "x"])).is_err());
        assert!(parse_options(&args(&["ri_a", "--since", "1h"])).is_err());
        assert_eq!(parse_options(&args(&[])).unwrap().issue, None);
    }

    #[test]
    fn the_repo_parameter_is_sent_only_when_given() {
        assert_eq!(
            lineage_path("acme", "ri_abc", None),
            "/api/cli/orgs/acme/runtime/issues/ri_abc/lineage"
        );
        assert_eq!(
            lineage_path("acme co", "ri_abc", Some("shop web")),
            "/api/cli/orgs/acme+co/runtime/issues/ri_abc/lineage?repo=shop+web"
        );
    }

    #[test]
    fn a_full_response_prints_the_chain_top_to_bottom() {
        let text = render(&full_response());
        let expected = "\
Issue ri_0123456789abcdef0123456789: TypeError: Cannot read properties of undefined (reading 'id')
  service checkout-api · environment production · status open · first seen 2026-11-14 09:12 UTC
  Repository: acme/shop

Release
  First seen in release v2.4.1
  Commit 3f9a2c1 (from GitHub release v2.4.1)
  https://github.com/acme/shop/commit/3f9a2c1d4e5f60718293a4b5c6d7e8f901234567
  Before it: release v2.4.0, commit 1a2b3c4

Pull request
  #482 Cache order lookups (from suspect commit S1)
  https://github.com/acme/shop/pull/482

Owner
  Assigned: Jane Doe, team payments, via CODEOWNERS: src/orders/ @acme/payments
  Also suggested:
    - Sam Lee (@samlee) (from blame)
      wrote 12 of the failing function's lines

Suspect commits
  S1 3f9a2c1 Cache order lookups
      by Jane Doe (@janedoe) with Claude · PR #482 · in the release window · 2026-11-13 18:00 UTC
      Why: Changed the failing line orders.ts:42.
  S2 9e8d7c6 Refactor order handler
      written by Claude, no human author · in the release window
      Why: Changed the failing function handler.

Who wrote the failing code
  src/orders.ts:42 in handler
    Claude (model claude-opus-4-5) in commit 3f9a2c1, edited by a person since
    Run by Jane Doe
    Function handler (lines 30-60): 25 of 31 lines written by AI
    Prompt: autter show-prompt s_3f9a2c1d4e5f60::t_0a1b2c3d4e5f60
  src/db.ts:10 in query
    Sam Lee in commit 1a2b3c4
    Line numbers are approximate.
  src/util.ts:5
    Not known: no authorship notes

Why this error was grouped into this issue
  Grouped by its top frames: handler@orders.ts, query@db.ts.

Notes
  - Owners were computed from CODEOWNERS at the release commit.
";
        assert_eq!(text, expected);
    }

    #[test]
    fn an_ai_only_suspect_has_no_human_author() {
        let text = render(&full_response());
        assert!(
            text.contains(
                "S2 9e8d7c6 Refactor order handler\n      written by Claude, no human author"
            ),
            "{text}"
        );
        let mut response = full_response();
        response["suspects"]["items"][1]["agents"] = json!([]);
        assert!(render(&response).contains("written by an AI agent, no human author"));
    }

    #[test]
    fn a_frame_without_notes_is_not_known_never_a_person() {
        let mut response = full_response();
        response["frameAuthorship"]["frames"][2]["note"] = Value::Null;
        let text = render(&response);
        assert!(
            text.contains("  src/util.ts:5\n    Not known: no authorship notes\n"),
            "{text}"
        );
        // Partial coverage without an owner is still not known.
        response["frameAuthorship"]["frames"][2]["coverage"] = json!("partial");
        let text = render(&response);
        assert!(text.contains("  src/util.ts:5\n    Not known\n"), "{text}");
        assert!(!text.contains("src/util.ts:5\n    A person"), "{text}");
    }

    #[test]
    fn a_frame_note_with_more_to_say_is_printed() {
        let mut response = full_response();
        response["frameAuthorship"]["frames"][2]["coverage"] = json!("partial");
        response["frameAuthorship"]["frames"][2]["note"] =
            json!("GitHub did not answer in time, so this file's history is incomplete.");
        let text = render(&response);
        assert!(
            text.contains("  src/util.ts:5\n    Not known\n    GitHub did not answer in time"),
            "{text}"
        );
    }

    #[test]
    fn best_effort_lines_are_labelled_and_exact_ones_are_not() {
        let text = render(&full_response());
        let orders = text.split("  src/db.ts").next().unwrap();
        assert!(!orders.contains("approximate"), "{text}");
        assert!(text.contains("  src/db.ts:10 in query\n    Sam Lee in commit 1a2b3c4\n    Line numbers are approximate."));
    }

    #[test]
    fn the_prompt_command_needs_a_session_and_a_stored_transcript() {
        let mut response = full_response();
        response["frameAuthorship"]["frames"][0]["owner"]["transcriptAvailable"] = json!(false);
        assert!(!render(&response).contains("autter show-prompt"));
        let mut response = full_response();
        response["frameAuthorship"]["frames"][0]["owner"]["sessionId"] = Value::Null;
        assert!(!render(&response).contains("autter show-prompt"));
    }

    #[test]
    fn a_null_part_prints_one_line_with_its_note() {
        let mut response = full_response();
        response["suspects"] = Value::Null;
        response["pr"] = Value::Null;
        response["owner"] = Value::Null;
        response["frameAuthorship"] = Value::Null;
        response["whyLinked"] = Value::Null;
        response["release"] = Value::Null;
        response["notes"] = json!([
            "The release this error first appeared in is not known.",
            "Suspect commits could not be read right now.",
            "Owners could not be read right now.",
            "Frame authorship could not be read right now.",
            "Why this occurrence joined the issue could not be read right now.",
            "Something else."
        ]);
        let text = render(&response);
        assert!(text.contains("Release\n  The release could not be read.\n  The release this error first appeared in is not known.\n"), "{text}");
        assert!(text.contains("Suspect commits\n  The suspect commits could not be read.\n  Suspect commits could not be read right now.\n"), "{text}");
        assert!(
            text.contains(
                "Owner\n  The owner could not be read.\n  Owners could not be read right now.\n"
            ),
            "{text}"
        );
        assert!(text.contains("  Who wrote the failing frames could not be read.\n  Frame authorship could not be read right now.\n"), "{text}");
        assert!(text.contains("  Why it was grouped could not be read.\n  Why this occurrence joined the issue could not be read right now.\n"), "{text}");
        assert!(
            text.contains("it comes from the suspect commits, which could not be read"),
            "{text}"
        );
        // Every note is printed once; the one no part claimed is listed at the end.
        assert!(text.ends_with("Notes\n  - Something else.\n"), "{text}");
        assert_eq!(
            text.matches("Owners could not be read right now.").count(),
            1
        );
    }

    #[test]
    fn an_issue_without_a_repository_says_so_under_each_part() {
        let mut response = full_response();
        for part in ["pr", "owner", "suspects", "frameAuthorship", "whyLinked"] {
            response[part] = Value::Null;
        }
        response["issue"]["repository"] = Value::Null;
        response["notes"] = json!([
            "The issue has no repository, so its commits, owners and code authorship could not be read."
        ]);
        let text = render(&response);
        assert!(
            text.contains("Owner\n  The owner could not be read.\n  The issue has no repository"),
            "{text}"
        );
        assert!(!text.contains("\nNotes\n"), "{text}");
    }

    #[test]
    fn an_unknown_commit_is_said_plainly() {
        let mut response = full_response();
        response["release"]["commit"] = Value::Null;
        response["release"]["resolved"] = json!(false);
        response["release"]["commitUrl"] = Value::Null;
        response["release"]["previous"] = Value::Null;
        response["notes"] = json!([
            "No commit is known for release v2.4.1, so code was read at the default branch."
        ]);
        let text = render(&response);
        assert!(text.contains("  First seen in release v2.4.1\n  Commit: not known, so code was read at the default branch.\n"), "{text}");
        assert!(!text.contains("No commit is known"), "{text}");
        assert!(!text.contains("Before it"), "{text}");
    }

    #[test]
    fn an_owner_is_assigned_else_suggested() {
        let mut response = full_response();
        response["owner"]["assigned"] = Value::Null;
        let text = render(&response);
        assert!(
            text.contains(
                "  No owner is assigned. Suggested:\n    - Sam Lee (@samlee) (from blame)"
            ),
            "{text}"
        );
        response["owner"]["suggestions"] = json!([]);
        assert!(render(&response).contains("  No owner is assigned and none is suggested."));
    }

    #[test]
    fn email_addresses_are_never_printed() {
        let mut response = full_response();
        response["owner"]["suggestions"][0]["via"] = json!(["CODEOWNERS: src/ jane@acme.com"]);
        response["owner"]["notes"] = json!(["Matched owner jane@acme.com"]);
        response["suspects"]["items"][0]["author"]["name"] = json!("<jane@acme.com>");
        let text = render(&response);
        assert!(!text.contains("jane@acme.com"), "{text}");
        assert!(text.contains("(email hidden)"), "{text}");
        // GitHub logins and team handles are not addresses.
        assert!(text.contains("@acme/payments"), "{text}");
        assert!(text.contains("@samlee"), "{text}");
    }

    #[test]
    fn terminal_control_characters_are_removed() {
        let mut response = full_response();
        response["issue"]["title"] = json!("boom\u{1b}[31m\nred");
        assert!(render(&response).contains("boom [31m red"));
    }

    /// The message an error prints after `error: `.
    fn message(error: AutterError) -> String {
        match error {
            AutterError::Generic(message) => message,
            other => other.to_string(),
        }
    }

    #[test]
    fn errors_are_mapped_to_what_to_do() {
        let error = response_value(401, r#"{"error":"Unauthenticated"}"#)
            .map_err(message)
            .unwrap_err();
        assert!(
            error.contains("autter login") && error.contains("--org"),
            "{error}"
        );
        let error = response_value(401, "").map_err(message).unwrap_err();
        assert!(error.contains("autter login"), "{error}");
        let error = response_value(404, r#"{"error":"Issue not found"}"#)
            .map_err(message)
            .unwrap_err();
        assert_eq!(error, ISSUE_NOT_FOUND);
        let error = response_value(404, r#"{"error":"Org not found or not a member."}"#)
            .map_err(message)
            .unwrap_err();
        assert_eq!(error, "HTTP 404: Org not found or not a member.");
        let error = response_value(400, r#"{"error":"Unknown repository: shop"}"#)
            .map_err(message)
            .unwrap_err();
        assert!(error.starts_with("Unknown repository: shop."), "{error}");
        assert!(error.contains("--repo"), "{error}");
        let error = response_value(500, r#"{"error":"Failed to load the issue's lineage."}"#)
            .map_err(message)
            .unwrap_err();
        assert_eq!(error, "HTTP 500: Failed to load the issue's lineage.");
        let error = response_value(502, "<html>Bad gateway</html>")
            .map_err(message)
            .unwrap_err();
        assert!(error.contains("HTTP 502"), "{error}");
        // A 200 that is not JSON is an error too, not an empty lineage.
        assert!(response_value(200, "<html>").is_err());
    }

    fn context(server: &mockito::Server) -> ApiContext {
        ApiContext::with_auth(Some(server.url()), "test-access-token".to_string())
    }

    #[test]
    fn fetch_sends_the_cli_token_and_returns_the_response_unchanged() {
        let mut server = mockito::Server::new();
        let body = full_response();
        let mock = server
            .mock(
                "GET",
                "/api/cli/orgs/acme/runtime/issues/ri_abc/lineage?repo=shop",
            )
            .match_header("authorization", "Bearer test-access-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body.to_string())
            .create();
        let options = parse_options(&args(&["ri_abc", "--repo", "shop"])).unwrap();
        let value = fetch(&context(&server), "acme", &options).unwrap();
        mock.assert();
        // `--json` prints this value: the server's document, every field kept.
        assert_eq!(value, body);
        let printed: Value =
            serde_json::from_str(&serde_json::to_string_pretty(&value).unwrap()).unwrap();
        assert_eq!(printed, body);
    }

    #[test]
    fn fetch_maps_http_errors() {
        let mut server = mockito::Server::new();
        let cases = [
            (401, r#"{"error":"Unauthenticated"}"#, "autter login"),
            (404, r#"{"error":"Issue not found"}"#, ISSUE_NOT_FOUND),
            (
                400,
                r#"{"error":"Unknown repository: nope"}"#,
                "Unknown repository: nope",
            ),
        ];
        for (status, body, expected) in cases {
            let mock = server
                .mock("GET", "/api/cli/orgs/acme/runtime/issues/ri_abc/lineage")
                .with_status(status)
                .with_body(body)
                .create();
            let options = parse_options(&args(&["ri_abc"])).unwrap();
            let error = fetch(&context(&server), "acme", &options)
                .unwrap_err()
                .to_string();
            mock.assert();
            mock.remove();
            assert!(error.contains(expected), "{status}: {error}");
        }
    }
}
