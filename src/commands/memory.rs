//! Repository memory from the Autter cloud: `autter memory search` ranks what
//! Autter knows about this repository by meaning (its shared memory pages,
//! learnings, production lessons, rules, past findings, the wiki and code), and
//! `autter memory show` prints the memory pages. Coding agents reach these
//! through the installed `repo-memory` skill when the Autter MCP server is not
//! connected. Nothing is sent in local mode or for repositories excluded from
//! prompt sharing.
use crate::commands::arg_parser::{self, ScanMode};
use crate::commands::ask::{component, org_api_context};
use crate::config::{Config, PromptStorageMode};
use crate::error::AutterError;
use crate::git::repository::discover_repository_in_path_no_git_exec;
use crate::repo_url::{normalize_repo_url, resolve_repo_url_from_repo};
use serde_json::{Map, Value, json};

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
        "show" => Ok(options),
        "" => Err("memory needs a subcommand: search or show".into()),
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
fn target_repository(explicit: Option<&str>) -> Result<(String, Option<Remotes>), AutterError> {
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
    let cwd = std::env::current_dir()?;
    let repo = discover_repository_in_path_no_git_exec(&cwd).map_err(|_| not_found())?;
    let url = resolve_repo_url_from_repo(&repo).ok_or_else(not_found)?;
    Ok((url, repo.remotes_with_urls().ok()))
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
    if !pages.is_empty() {
        out.push_str("\nPages:\n");
        let width = pages
            .iter()
            .filter_map(|page| page["slug"].as_str())
            .map(|slug| slug.chars().count())
            .max()
            .unwrap_or(0);
        for page in pages {
            out.push_str(&format!(
                "  {:<width$}  {}\n",
                clean(page["slug"].as_str().unwrap_or("")),
                clean(page["title"].as_str().unwrap_or("")),
            ));
        }
    }
    out
}

fn run(options: Options) -> Result<(), AutterError> {
    let config = Config::get();
    let (repository, remotes) = target_repository(options.repository.as_deref())?;
    if let Some(reason) = sharing_refusal(
        config.platform_sync_enabled(),
        config.effective_prompt_storage_with_remotes(remotes.as_ref()),
    ) {
        return Err(AutterError::Generic(reason.into()));
    }
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
        if let Some(page) = options.words.first() {
            params.append_pair("slug", page);
        }
        response_json(context.get(&format!("{base}?{}", params.finish()))?)?
    };
    if arg_parser::json() {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if options.subcommand == "search" {
        print!("{}", render_search(&result));
        let has_pages = result["results"]
            .as_array()
            .is_some_and(|hits| hits.iter().any(|hit| hit["kind"] == "repo_memory_page"));
        if has_pages && !arg_parser::quiet() {
            println!("\nOpen a memory page with: autter memory show <page>");
        }
    } else {
        print!("{}", render_show(&result));
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
        eprintln!("error: {error}");
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
        let (repo, remotes) = target_repository(Some("git@github.com:acme/api.git")).unwrap();
        assert_eq!(repo, "https://github.com/acme/api");
        assert_eq!(
            remotes,
            Some(vec![(
                "--repo".into(),
                "https://github.com/acme/api".into()
            )])
        );
        let (repo, remotes) = target_repository(Some("acme/api")).unwrap();
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
}
