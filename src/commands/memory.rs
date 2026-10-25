//! Repository memory from the Autter cloud: `autter memory search` ranks what
//! Autter knows about this repository by meaning (its shared memory pages,
//! learnings, production lessons, rules, past findings, the wiki and code), and
//! `autter memory show` prints the memory pages. The memory keeps its detail in
//! one page per area of the codebase and a what-works ledger, so
//! `autter memory for [<file>...]` prints the area pages covering the files at
//! hand (by default the changed ones) with the ledger lines about those areas,
//! the way reviews read them; `areas` and `ledger` list them. Coding agents reach these
//! through the installed `repo-memory` skill when the Autter MCP server is not
//! connected. Nothing is sent in local mode or for repositories excluded from
//! prompt sharing.
use crate::commands::arg_parser::{self, ScanMode};
use crate::commands::ask::{component, org_api_context};
use crate::config::{Config, PromptStorageMode};
use crate::error::AutterError;
use crate::git::repository::{Repository, discover_repository_in_path_no_git_exec};
use crate::repo_url::{normalize_repo_url, resolve_repo_url_from_repo};
use serde_json::{Map, Value, json};
use std::path::{Component, Path, PathBuf};

/// Search sources the server accepts (`search_context` kinds in the platform).
const KINDS: &[&str] = &[
    "repo_memory",
    "learnings",
    "lessons",
    "rules",
    "memory",
    "repo_context",
    "review_findings",
    "scan_findings",
    "docs",
    "incidents",
    "pr_summaries",
    "tickets",
    "code",
    "symbols",
];

const LOCAL_MODE: &str = "Repository memory lives in the Autter cloud, and this machine is in local mode, so nothing is sent to Autter. Run `autter onboard` to connect.";
const EXCLUDED: &str = "This repository is excluded from sharing with Autter (exclude_prompts_in_repositories), so it has no cloud memory and its questions are not sent.";
const EXCERPT_CHARS: usize = 280;
/// Files one `autter memory for` asks about (the server's limit).
const MAX_FILES: usize = 20;
/// The what-works ledger page the nightly pass keeps (RM-10 in the platform).
const LEDGER_SLUG: &str = "ledger/what-works";
const NO_LEDGER: &str = "This repository's memory has no what-works ledger yet. Autter starts it the first night it sees changes kept, reworked or waved off.";

#[derive(Debug, Default, PartialEq)]
struct Options {
    subcommand: String,
    words: Vec<String>,
    repository: Option<String>,
    organization: Option<String>,
    paths: Vec<String>,
    kinds: Vec<String>,
    limit: Option<u32>,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            options.words.extend_from_slice(&args[index + 1..]);
            break;
        }
        if !arg.starts_with("--") {
            if options.subcommand.is_empty() {
                options.subcommand = arg.to_owned();
            } else {
                options.words.push(arg.to_owned());
            }
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| format!("{arg} requires a value"))?;
        match arg {
            "--repo" => options.repository = Some(value.clone()),
            "--org" => options.organization = Some(value.clone()),
            "--path" if options.subcommand == "search" => options.paths.push(value.clone()),
            "--kind" if options.subcommand == "search" => {
                for kind in value.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                    if !KINDS.contains(&kind) {
                        return Err(format!("--kind must be one of: {}", KINDS.join(", ")));
                    }
                    options.kinds.push(kind.to_owned());
                }
            }
            "--limit" if options.subcommand == "search" => {
                let limit: u32 = value
                    .parse()
                    .map_err(|_| "--limit must be between 1 and 30")?;
                if !(1..=30).contains(&limit) {
                    return Err("--limit must be between 1 and 30".into());
                }
                options.limit = Some(limit);
            }
            _ => {
                let command = if options.subcommand.is_empty() {
                    "memory".to_owned()
                } else {
                    format!("memory {}", options.subcommand)
                };
                return Err(format!("unknown option for {command}: {arg}"));
            }
        }
        index += 2;
    }
    match options.subcommand.as_str() {
        "search" if options.words.join(" ").trim().chars().count() < 2 => {
            Err("memory search needs a question of at least 2 characters".into())
        }
        "search" => Ok(options),
        "show" if options.words.len() > 1 => Err("memory show takes at most one page".into()),
        "for" if options.words.len() > MAX_FILES => {
            Err(format!("memory for takes at most {MAX_FILES} files"))
        }
        "show" | "for" => Ok(options),
        "areas" | "ledger" if !options.words.is_empty() => {
            Err(format!("memory {} takes no arguments", options.subcommand))
        }
        "areas" | "ledger" => Ok(options),
        "" => Err("memory needs a subcommand: search, for, show, areas or ledger".into()),
        other => Err(format!("unknown memory subcommand: {other}")),
    }
}

/// Why the cloud must not be asked about this repository, if it must not.
fn sharing_refusal(platform_sync: bool, storage: PromptStorageMode) -> Option<&'static str> {
    if !platform_sync {
        Some(LOCAL_MODE)
    } else if storage == PromptStorageMode::Local {
        Some(EXCLUDED)
    } else {
        None
    }
}

/// `(name, url)` pairs, as `Repository::remotes_with_urls` returns them.
type Remotes = Vec<(String, String)>;

/// The repository to ask about (`--repo`, else this checkout's default remote,
/// normalized to https) and the remotes the sharing policy is checked against.
fn target_repository(
    explicit: Option<&str>,
    local: Option<&Repository>,
) -> Result<(String, Option<Remotes>), AutterError> {
    if let Some(value) = explicit {
        return Ok(match normalize_repo_url(value) {
            Ok(url) => (url.clone(), Some(vec![("--repo".to_owned(), url)])),
            Err(_) => (value.trim().to_owned(), None),
        });
    }
    let not_found = || {
        AutterError::Generic(
            "Run this inside a git checkout with a remote, or pass --repo <owner/repo>.".into(),
        )
    };
    let repo = local.ok_or_else(not_found)?;
    let url = resolve_repo_url_from_repo(repo).ok_or_else(not_found)?;
    Ok((url, repo.remotes_with_urls().ok()))
}

/// `path` relative to the checkout root, with `/` separators; `cwd` resolves a
/// relative path. Lexical, so a file that does not exist yet still resolves.
fn repo_relative(workdir: &Path, cwd: &Path, path: &str) -> Option<String> {
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    let mut normal = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    let rest = normal.strip_prefix(workdir).ok()?;
    let text = rest
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    Some(text)
}

/// The files `autter memory for` asks about: the arguments, made relative to
/// the checkout root, or the files changed in the working tree when none are
/// given. Outside a checkout (with `--repo`) arguments are taken as
/// repository paths.
fn files_for(words: &[String], local: Option<&Repository>) -> Result<Vec<String>, AutterError> {
    let mut files: Vec<String> = match local {
        Some(repo) if !words.is_empty() => {
            let cwd = std::env::current_dir()?;
            let workdir = repo.workdir()?;
            // Compare like with like: canonical root, canonical cwd.
            let workdir = workdir.canonicalize().unwrap_or(workdir);
            let cwd = cwd.canonicalize().unwrap_or(cwd);
            let mut out = Vec::new();
            for word in words {
                // An absolute path must be canonical too to share the root's prefix.
                let canonical = Path::new(word)
                    .is_absolute()
                    .then(|| Path::new(word).canonicalize().ok())
                    .flatten()
                    .map(|path| path.to_string_lossy().into_owned());
                let file = repo_relative(&workdir, &cwd, canonical.as_deref().unwrap_or(word))
                    .ok_or_else(|| {
                        AutterError::Generic(format!("{word} is outside this repository."))
                    })?;
                out.push(file);
            }
            out
        }
        Some(repo) => {
            let mut changed: Vec<String> = repo
                .get_staged_and_unstaged_filenames()?
                .into_iter()
                .collect();
            changed.sort();
            changed
        }
        None => words
            .iter()
            .map(|w| w.replace('\\', "/").trim_start_matches("./").to_owned())
            .collect(),
    };
    files.retain(|file| !file.is_empty());
    files.dedup();
    if files.is_empty() {
        return Err(AutterError::Generic(
            "Nothing changed in this checkout. Name the files: autter memory for <file>...".into(),
        ));
    }
    files.truncate(MAX_FILES);
    Ok(files)
}

fn response_json(response: crate::http::Response) -> Result<Value, AutterError> {
    let code = response.status_code;
    let value: Option<Value> = response
        .as_str()
        .ok()
        .and_then(|text| serde_json::from_str(text).ok());
    if code == 200
        && let Some(value) = value
    {
        return Ok(value);
    }
    let value = value.unwrap_or(Value::Null);
    let message = value["message"]
        .as_str()
        .or_else(|| value["error"].as_str())
        .map(str::to_owned);
    let message = match (code, message) {
        (401, _) => "Your Autter sign-in has expired. Run `autter login`.".to_owned(),
        (404, None) => {
            "This Autter deployment does not serve repository memory to the CLI yet.".to_owned()
        }
        (_, Some(message)) => {
            let repos: Vec<&str> = value["detail"]["available_repos"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .take(10)
                .collect();
            if repos.is_empty() {
                message
            } else {
                format!("{message} Connected: {}.", repos.join(", "))
            }
        }
        (_, None) => format!("Repository memory request failed (HTTP {code})."),
    };
    Err(AutterError::Generic(message))
}

/// Text from the server, without terminal control sequences.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// One line of at most `max` characters.
fn excerpt(text: &str, max: usize) -> String {
    let flat = clean(text).split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max - 1).collect::<String>())
    } else {
        flat
    }
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "repo_memory_page" => "memory",
        "org_memory_page" => "org memory",
        "learning" => "learning",
        "lesson" => "lesson",
        "team_rule" => "rule",
        "code_file" => "file",
        "code_symbol" => "symbol",
        "review_finding" => "review",
        "scan_finding" => "scan",
        "doc" => "wiki",
        "context_source" | "repo_context" => "context",
        "runtime_incident" => "incident",
        "ci_failure" => "ci",
        "pr_summary" => "pr",
        other => other,
    }
}

fn render_search(result: &Value) -> String {
    let empty = Vec::new();
    let results = result["results"].as_array().unwrap_or(&empty);
    let repo = clean(result["repo"].as_str().unwrap_or(""));
    let mut out = String::new();
    if results.is_empty() {
        out.push_str(&format!("Nothing in {repo}'s memory matches that.\n"));
    } else {
        out.push_str(&format!(
            "{repo}: {} result{}\n",
            results.len(),
            if results.len() == 1 { "" } else { "s" }
        ));
    }
    for (rank, hit) in results.iter().enumerate() {
        let title = hit["title"]
            .as_str()
            .or_else(|| hit["path"].as_str())
            .unwrap_or("(untitled)");
        let score = hit["score"]
            .as_f64()
            .map(|score| format!("  {score:.2}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n{:>2}. [{}] {}{score}\n",
            rank + 1,
            kind_label(hit["kind"].as_str().unwrap_or("")),
            excerpt(title, 100)
        ));
        let mut place = Vec::new();
        if let Some(slug) = hit["metadata"]["slug"].as_str() {
            place.push(format!("page {}", clean(slug)));
        }
        if let Some(path) = hit["path"].as_str()
            && Some(path) != hit["title"].as_str()
        {
            place.push(clean(path));
        }
        if let Some(line) = hit["metadata"]["line"].as_u64() {
            place.push(format!("line {line}"));
        }
        if !place.is_empty() {
            out.push_str(&format!("    {}\n", place.join(" · ")));
        }
        let text = excerpt(hit["excerpt"].as_str().unwrap_or(""), EXCERPT_CHARS);
        if !text.is_empty() {
            out.push_str(&format!("    {text}\n"));
        }
    }
    if result["mode"] == "literal" {
        let reason = result["fallback_reason"]
            .as_str()
            .unwrap_or("the semantic index had no matches");
        out.push_str(&format!(
            "\nMatched by words, not meaning: {}.\n",
            clean(reason)
        ));
    }
    if result["has_more"] == true {
        out.push_str("\nMore results match. Raise --limit (up to 30) or narrow with --kind.\n");
    }
    out
}

fn render_page(repo: &str, page: &Value) -> String {
    let mut out = format!(
        "{} ({}, {})\n",
        clean(page["title"].as_str().unwrap_or("")),
        clean(page["slug"].as_str().unwrap_or("")),
        clean(repo)
    );
    let body = clean(page["body"].as_str().unwrap_or("")).trim().to_owned();
    if !body.is_empty() {
        out.push('\n');
        out.push_str(&body);
        out.push('\n');
    }
    out
}

/// `(slug, label)` rows as an aligned two-column list.
fn render_rows(out: &mut String, rows: &[(String, String)]) {
    let width = rows
        .iter()
        .map(|(slug, _)| slug.chars().count())
        .max()
        .unwrap_or(0);
    for (slug, label) in rows {
        out.push_str(&format!("  {slug:<width$}  {label}\n"));
    }
}

/// An area's path for people: the prefix, or "whole repository" for the root.
fn area_path_label(path: &Value) -> String {
    match path.as_str() {
        Some("") => "whole repository".to_owned(),
        Some(path) => clean(path),
        None => "no path".to_owned(),
    }
}

fn area_rows(areas: &[Value]) -> Vec<(String, String)> {
    areas
        .iter()
        .map(|area| {
            (
                clean(area["slug"].as_str().unwrap_or("")),
                format!(
                    "{}  ({})",
                    clean(area["title"].as_str().unwrap_or("")),
                    area_path_label(&area["path"])
                ),
            )
        })
        .collect()
}

fn render_areas(result: &Value) -> String {
    let repo = clean(result["repo"].as_str().unwrap_or(""));
    let empty = Vec::new();
    let areas = result["areas"].as_array().unwrap_or(&empty);
    if areas.is_empty() {
        return format!(
            "{repo}'s memory has no area pages yet. Autter writes them each night for the parts of the codebase with activity.\n"
        );
    }
    let mut out = format!("{repo}: {} areas\n\n", areas.len());
    render_rows(&mut out, &area_rows(areas));
    out
}

fn render_show(result: &Value) -> String {
    let repo = result["repo"].as_str().unwrap_or("");
    if result["page"].is_object() {
        return render_page(repo, &result["page"]);
    }
    let empty = Vec::new();
    let pages = result["pages"].as_array().unwrap_or(&empty);
    if result["home"]["exists"] != true && pages.is_empty() {
        return format!(
            "{} has no repository memory yet. Autter writes it each night from agent sessions, reviews and learnings.\n",
            clean(repo)
        );
    }
    let mut out = render_page(repo, &result["home"]);
    let areas = result["areas"].as_array().unwrap_or(&empty);
    if !areas.is_empty() {
        out.push_str("\nAreas (autter memory for <file> opens the ones covering it):\n");
        render_rows(&mut out, &area_rows(areas));
    }
    let others: Vec<(String, String)> = pages
        .iter()
        .filter(|page| !areas.iter().any(|area| area["slug"] == page["slug"]))
        .map(|page| {
            (
                clean(page["slug"].as_str().unwrap_or("")),
                clean(page["title"].as_str().unwrap_or("")),
            )
        })
        .collect();
    if !others.is_empty() {
        out.push_str("\nPages:\n");
        render_rows(&mut out, &others);
    }
    out
}

fn plural(count: usize, one: &str) -> String {
    format!("{count} {one}{}", if count == 1 { "" } else { "s" })
}

/// The memory for the files at hand: the area pages covering them, most
/// specific first, then what the ledger says about those areas.
fn render_for(result: &Value) -> String {
    let repo = clean(result["repo"].as_str().unwrap_or(""));
    let empty = Vec::new();
    let paths = result["paths"].as_array().unwrap_or(&empty);
    let areas = result["areas"].as_array().unwrap_or(&empty);
    let uncovered = result["uncovered"].as_array().unwrap_or(&empty);
    if result["has_memory"] == false {
        return format!(
            "{repo} has no repository memory yet. Autter writes it each night from agent sessions, reviews and learnings.\n"
        );
    }
    let mut out = if areas.is_empty() {
        format!(
            "No area page in {repo}'s memory covers {}. List them with `autter memory areas`, or search with `autter memory search \"<question>\"`.\n",
            if paths.len() == 1 {
                "this file"
            } else {
                "these files"
            }
        )
    } else {
        format!(
            "{repo}: {} for {}\n",
            plural(areas.len(), "area page"),
            plural(paths.len(), "file")
        )
    };
    for area in areas {
        let covers: Vec<String> = area["covers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(clean)
            .collect();
        out.push_str(&format!(
            "\n== {} ({}, {})\n   covers {}\n",
            clean(area["title"].as_str().unwrap_or("")),
            clean(area["slug"].as_str().unwrap_or("")),
            area_path_label(&area["path"]),
            covers.join(", ")
        ));
        let body = clean(area["body"].as_str().unwrap_or("")).trim().to_owned();
        if !body.is_empty() {
            out.push('\n');
            out.push_str(&body);
            out.push('\n');
        }
    }
    let lines = result["ledger"]["lines"].as_array().unwrap_or(&empty);
    if !lines.is_empty() {
        out.push_str("\nWhat the ledger says about these areas:\n");
        let mut section: Option<&str> = None;
        for line in lines {
            let this = line["section"].as_str();
            if this != section {
                if let Some(name) = this {
                    out.push_str(&format!("  {}:\n", clean(name)));
                }
                section = this;
            }
            out.push_str(&format!(
                "    - {}\n",
                excerpt(line["line"].as_str().unwrap_or(""), 400)
            ));
        }
    }
    if !uncovered.is_empty() && !areas.is_empty() {
        let names: Vec<String> = uncovered
            .iter()
            .filter_map(Value::as_str)
            .map(clean)
            .collect();
        out.push_str(&format!("\nNo area page covers: {}\n", names.join(", ")));
    }
    out
}

fn run(options: Options) -> Result<(), AutterError> {
    let config = Config::get();
    let local = std::env::current_dir()
        .ok()
        .and_then(|cwd| discover_repository_in_path_no_git_exec(&cwd).ok());
    let (repository, remotes) = target_repository(options.repository.as_deref(), local.as_ref())?;
    if let Some(reason) = sharing_refusal(
        config.platform_sync_enabled(),
        config.effective_prompt_storage_with_remotes(remotes.as_ref()),
    ) {
        return Err(AutterError::Generic(reason.into()));
    }
    // With --repo, this checkout (if any) says nothing about that repository's files.
    let files_repo = if options.repository.is_some() {
        None
    } else {
        local.as_ref()
    };
    let files = if options.subcommand == "for" {
        files_for(&options.words, files_repo)?
    } else {
        Vec::new()
    };
    let (context, slug) = org_api_context(options.organization.as_deref(), 60)?;
    let base = format!("/api/cli/orgs/{}/memory", component(&slug));
    let result = if options.subcommand == "search" {
        let mut body = Map::new();
        body.insert("repository".into(), json!(repository));
        body.insert("query".into(), json!(options.words.join(" ").trim()));
        if !options.paths.is_empty() {
            body.insert("paths".into(), json!(options.paths));
        }
        if !options.kinds.is_empty() {
            body.insert("kinds".into(), json!(options.kinds));
        }
        if let Some(limit) = options.limit {
            body.insert("limit".into(), json!(limit));
        }
        response_json(context.post_json(&format!("{base}/search"), &Value::Object(body))?)?
    } else {
        let mut params = url::form_urlencoded::Serializer::new(String::new());
        params.append_pair("repository", &repository);
        match options.subcommand.as_str() {
            "show" => {
                if let Some(page) = options.words.first() {
                    params.append_pair("slug", page);
                }
            }
            "ledger" => {
                params.append_pair("slug", LEDGER_SLUG);
            }
            "for" => {
                for file in &files {
                    params.append_pair("path", file);
                }
            }
            _ => {}
        }
        match response_json(context.get(&format!("{base}?{}", params.finish()))?) {
            Err(AutterError::Generic(message))
                if options.subcommand == "ledger"
                    && message.contains("No repository memory page") =>
            {
                return Err(AutterError::Generic(NO_LEDGER.into()));
            }
            other => other?,
        }
    };
    if arg_parser::json() {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    match options.subcommand.as_str() {
        "search" => {
            print!("{}", render_search(&result));
            let has_pages = result["results"]
                .as_array()
                .is_some_and(|hits| hits.iter().any(|hit| hit["kind"] == "repo_memory_page"));
            if has_pages && !arg_parser::quiet() {
                println!("\nOpen a memory page with: autter memory show <page>");
            }
        }
        "for" => print!("{}", render_for(&result)),
        "areas" => print!("{}", render_areas(&result)),
        _ => print!("{}", render_show(&result)),
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
        arg_parser::print_command_help("memory");
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
        arg_parser::print_command_help("memory");
        std::process::exit(2);
    });
    if let Err(error) = run(options) {
        match error {
            AutterError::Generic(message) => eprintln!("error: {message}"),
            other => eprintln!("error: {other}"),
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn search_options_take_a_question_and_filters() {
        let options = parse_options(&args(&[
            "search",
            "how",
            "do",
            "we",
            "retry",
            "--path",
            "src/http/",
            "--kind",
            "repo_memory,lessons",
            "--kind",
            "rules",
            "--limit",
            "5",
            "--repo",
            "acme/api",
        ]))
        .unwrap();
        assert_eq!(options.words.join(" "), "how do we retry");
        assert_eq!(options.paths, vec!["src/http/"]);
        assert_eq!(options.kinds, vec!["repo_memory", "lessons", "rules"]);
        assert_eq!(options.limit, Some(5));
        assert_eq!(options.repository.as_deref(), Some("acme/api"));
    }

    #[test]
    fn options_are_validated_before_anything_is_sent() {
        assert!(parse_options(&args(&["search"])).is_err());
        assert!(parse_options(&args(&["search", "x"])).is_err());
        assert!(parse_options(&args(&["search", "retry", "--kind", "secrets"])).is_err());
        assert!(parse_options(&args(&["search", "retry", "--limit", "31"])).is_err());
        assert!(parse_options(&args(&["search", "retry", "--limit", "0"])).is_err());
        assert!(parse_options(&args(&["search", "retry", "--repo"])).is_err());
        assert!(parse_options(&args(&["show", "--limit", "5"])).is_err());
        assert!(parse_options(&args(&["show", "a", "b"])).is_err());
        assert!(parse_options(&args(&["forget"])).is_err());
        assert!(parse_options(&args(&[])).is_err());
        let show = parse_options(&args(&["show", "gotchas/payments"])).unwrap();
        assert_eq!(show.words, vec!["gotchas/payments"]);
    }

    #[test]
    fn local_mode_and_excluded_repositories_send_nothing() {
        assert_eq!(
            sharing_refusal(false, PromptStorageMode::Local),
            Some(LOCAL_MODE)
        );
        assert_eq!(
            sharing_refusal(false, PromptStorageMode::Default),
            Some(LOCAL_MODE)
        );
        assert_eq!(
            sharing_refusal(true, PromptStorageMode::Local),
            Some(EXCLUDED)
        );
        assert_eq!(sharing_refusal(true, PromptStorageMode::Default), None);
        assert_eq!(sharing_refusal(true, PromptStorageMode::Notes), None);
    }

    #[test]
    fn an_explicit_repository_is_normalized_like_uploads() {
        let (repo, remotes) = target_repository(Some("git@github.com:acme/api.git"), None).unwrap();
        assert_eq!(repo, "https://github.com/acme/api");
        assert_eq!(
            remotes,
            Some(vec![(
                "--repo".into(),
                "https://github.com/acme/api".into()
            )])
        );
        let (repo, remotes) = target_repository(Some("acme/api"), None).unwrap();
        assert_eq!(repo, "acme/api");
        assert_eq!(remotes, None);
    }

    #[test]
    fn search_results_render_ranked_with_where_to_look() {
        let result = json!({
            "repo": "acme/api",
            "mode": "hybrid",
            "has_more": true,
            "results": [
                {
                    "kind": "repo_memory_page",
                    "title": "Repository memory › Gotchas",
                    "path": "src/billing/webhooks.ts",
                    "excerpt": "Stripe webhooks\narrive twice;\u{1b}[31m dedupe on the event id.",
                    "score": 0.912,
                    "metadata": { "slug": "MEMORY" }
                },
                {
                    "kind": "code_symbol",
                    "title": "retryWithBackoff",
                    "path": "src/http/retry.ts",
                    "excerpt": "function retryWithBackoff",
                    "score": null,
                    "metadata": { "line": 12 }
                }
            ]
        });
        assert_eq!(
            render_search(&result),
            "acme/api: 2 results\n\
             \n 1. [memory] Repository memory › Gotchas  0.91\
             \n    page MEMORY · src/billing/webhooks.ts\
             \n    Stripe webhooks arrive twice;[31m dedupe on the event id.\n\
             \n 2. [symbol] retryWithBackoff\
             \n    src/http/retry.ts · line 12\
             \n    function retryWithBackoff\n\
             \nMore results match. Raise --limit (up to 30) or narrow with --kind.\n"
        );
    }

    #[test]
    fn literal_fallback_and_empty_results_say_so() {
        let result = json!({
            "repo": "acme/api",
            "mode": "literal",
            "fallback_reason": "semantic index is empty for this repository",
            "results": []
        });
        assert_eq!(
            render_search(&result),
            "Nothing in acme/api's memory matches that.\n\
             \nMatched by words, not meaning: semantic index is empty for this repository.\n"
        );
    }

    #[test]
    fn show_renders_the_home_page_and_index_or_one_page() {
        let home = json!({
            "repo": "acme/api",
            "home": { "slug": "MEMORY", "title": "Repository memory", "body": "## Conventions\n- Use the query builder.", "exists": true },
            "pages": [
                { "slug": "gotchas/payments", "title": "Payments" },
                { "slug": "workflows", "title": "Workflows" }
            ]
        });
        assert_eq!(
            render_show(&home),
            "Repository memory (MEMORY, acme/api)\n\
             \n## Conventions\n- Use the query builder.\n\
             \nPages:\n  gotchas/payments  Payments\n  workflows         Workflows\n"
        );
        let page = json!({
            "repo": "acme/api",
            "page": { "slug": "workflows", "title": "Workflows", "body": "Tag from main." }
        });
        assert_eq!(
            render_show(&page),
            "Workflows (workflows, acme/api)\n\nTag from main.\n"
        );
        let none = json!({ "repo": "acme/api", "home": { "exists": false }, "pages": [] });
        assert!(render_show(&none).starts_with("acme/api has no repository memory yet."));
    }

    #[test]
    fn structured_subcommands_take_files_or_nothing() {
        let options = parse_options(&args(&["for", "src/a.rs", "src/b/"])).unwrap();
        assert_eq!(options.words, vec!["src/a.rs", "src/b/"]);
        assert!(parse_options(&args(&["for"])).unwrap().words.is_empty());
        let too_many: Vec<String> = std::iter::once("for".to_owned())
            .chain((0..21).map(|i| format!("f{i}")))
            .collect();
        assert!(parse_options(&too_many).is_err());
        assert!(parse_options(&args(&["for", "a", "--kind", "code"])).is_err());
        assert!(parse_options(&args(&["areas"])).is_ok());
        assert!(parse_options(&args(&["areas", "x"])).is_err());
        assert!(parse_options(&args(&["ledger", "--repo", "acme/api"])).is_ok());
        assert!(parse_options(&args(&["ledger", "x"])).is_err());
    }

    #[test]
    fn files_resolve_relative_to_the_checkout_root() {
        let root = Path::new("/work/app");
        let cwd = Path::new("/work/app/src/payments");
        assert_eq!(
            repo_relative(root, cwd, "charge.rs").as_deref(),
            Some("src/payments/charge.rs")
        );
        assert_eq!(
            repo_relative(root, cwd, "../http/./retry.rs").as_deref(),
            Some("src/http/retry.rs")
        );
        assert_eq!(
            repo_relative(root, cwd, ".").as_deref(),
            Some("src/payments")
        );
        assert_eq!(repo_relative(root, cwd, "../../../other/x.rs"), None);
    }

    #[test]
    fn memory_for_files_prints_area_pages_then_the_ledger() {
        let result = json!({
            "repo": "acme/api",
            "paths": ["src/payments/webhooks/stripe.ts", "README.md"],
            "has_memory": true,
            "areas": [
                {
                    "slug": "areas/src-payments-webhooks",
                    "title": "Payment webhooks",
                    "path": "src/payments/webhooks/",
                    "covers": ["src/payments/webhooks/stripe.ts"],
                    "body": "# Payment webhooks\nPath: `src/payments/webhooks/`\n\n## Conventions\n- Dedupe on event id."
                },
                {
                    "slug": "areas/root",
                    "title": "Repository root",
                    "path": "",
                    "covers": ["src/payments/webhooks/stripe.ts"],
                    "body": ""
                }
            ],
            "uncovered": ["README.md"],
            "ledger": { "lines": [
                { "section": "Works", "line": "Idempotency keys [src-payments-webhooks]", "area": "src-payments-webhooks" },
                { "section": "Doesn't work", "line": "Polling Stripe [src-payments-webhooks]", "area": "src-payments-webhooks" },
                { "section": "Doesn't work", "line": "Secrets in env [root]", "area": "root" }
            ] }
        });
        assert_eq!(
            render_for(&result),
            "acme/api: 2 area pages for 2 files\n\
             \n== Payment webhooks (areas/src-payments-webhooks, src/payments/webhooks/)\
             \n   covers src/payments/webhooks/stripe.ts\n\
             \n# Payment webhooks\nPath: `src/payments/webhooks/`\n\n## Conventions\n- Dedupe on event id.\n\
             \n== Repository root (areas/root, whole repository)\
             \n   covers src/payments/webhooks/stripe.ts\n\
             \nWhat the ledger says about these areas:\
             \n  Works:\
             \n    - Idempotency keys [src-payments-webhooks]\
             \n  Doesn't work:\
             \n    - Polling Stripe [src-payments-webhooks]\
             \n    - Secrets in env [root]\n\
             \nNo area page covers: README.md\n"
        );
        let none = json!({ "repo": "acme/api", "paths": ["README.md"], "has_memory": true, "areas": [], "uncovered": ["README.md"], "ledger": null });
        assert!(
            render_for(&none).starts_with("No area page in acme/api's memory covers this file.")
        );
        let empty = json!({ "repo": "acme/api", "paths": ["a"], "has_memory": false, "areas": [] });
        assert!(render_for(&empty).starts_with("acme/api has no repository memory yet."));
    }

    #[test]
    fn areas_list_with_their_paths_and_show_groups_them() {
        let home = json!({
            "repo": "acme/api",
            "home": { "slug": "MEMORY", "title": "Repository memory", "body": "- Map.", "exists": true },
            "pages": [
                { "slug": "areas/root", "title": "Root" },
                { "slug": "areas/src-http", "title": "HTTP client" },
                { "slug": "ledger/what-works", "title": "What works" }
            ],
            "areas": [
                { "slug": "areas/root", "title": "Root", "path": "" },
                { "slug": "areas/src-http", "title": "HTTP client", "path": "src/http/" }
            ]
        });
        assert_eq!(
            render_areas(&home),
            "acme/api: 2 areas\n\n  areas/root      Root  (whole repository)\n  areas/src-http  HTTP client  (src/http/)\n"
        );
        assert_eq!(
            render_show(&home),
            "Repository memory (MEMORY, acme/api)\n\n- Map.\n\
             \nAreas (autter memory for <file> opens the ones covering it):\
             \n  areas/root      Root  (whole repository)\
             \n  areas/src-http  HTTP client  (src/http/)\n\
             \nPages:\n  ledger/what-works  What works\n"
        );
        let bare = json!({ "repo": "acme/api", "areas": [] });
        assert!(render_areas(&bare).starts_with("acme/api's memory has no area pages yet."));
    }
}
