//! `autter context "<task>"`: what Autter knows that matters for a task, in
//! one bounded call.
//!
//! The CLI side of the platform's `get_context` MCP tool, for agents that are
//! not connected to the Autter MCP server. It posts the task (with the files
//! at hand, by default the ones changed in this checkout) to
//! `POST /api/cli/orgs/<org>/context` and prints the answer as Markdown: the
//! repository memory facts (`rmf_…` ids, to cite), wiki pages and code
//! structure that matter for it, ranked and cut to a token budget, each with
//! where to read more. `--since <cursor>` leaves out what an earlier call
//! already returned unchanged. Nothing is sent in local mode or for
//! repositories excluded from prompt sharing.
use crate::commands::arg_parser::{self, ScanMode};
use crate::commands::ask::{component, org_api_context};
use crate::commands::memory::{
    clean, excerpt, resolve_files, response_json_or, sharing_refusal, target_repository,
};
use crate::config::Config;
use crate::error::AutterError;
use crate::git::repository::discover_repository_in_path_no_git_exec;
use serde_json::{Map, Value, json};

/// The stores a context call can draw on.
const STORES: &[&str] = &["memory", "wiki", "code"];
/// Token budget the server accepts.
const BUDGET: std::ops::RangeInclusive<u32> = 300..=16_000;
/// Paths or symbols one call may name (the server's limit).
const MAX_NAMES: usize = 20;
/// Characters of task text the server accepts.
const MAX_TASK_CHARS: usize = 2_000;

const MISSING_ROUTE: &str = "This Autter deployment does not serve `autter context` yet (POST /context). Use `autter memory for` and `autter memory search` until it is updated.";

#[derive(Debug, Default, PartialEq)]
struct Options {
    words: Vec<String>,
    paths: Vec<String>,
    symbols: Vec<String>,
    budget: Option<u32>,
    include: Vec<String>,
    since: Option<String>,
    repository: Option<String>,
    organization: Option<String>,
}

impl Options {
    fn task(&self) -> String {
        self.words.join(" ").trim().to_owned()
    }
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
            options.words.push(arg.to_owned());
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| format!("{arg} requires a value"))?;
        match arg {
            "--path" => options.paths.push(value.clone()),
            "--symbol" => {
                let symbol = value.trim();
                if symbol.is_empty() {
                    return Err("--symbol needs a name".into());
                }
                options.symbols.push(symbol.to_owned());
            }
            "--budget" => {
                let budget: u32 = value
                    .parse()
                    .ok()
                    .filter(|budget| BUDGET.contains(budget))
                    .ok_or("--budget must be a number of tokens from 300 to 16000")?;
                options.budget = Some(budget);
            }
            "--include" => {
                for store in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if !STORES.contains(&store) {
                        return Err(format!("--include must be from: {}", STORES.join(",")));
                    }
                    if !options.include.iter().any(|s| s == store) {
                        options.include.push(store.to_owned());
                    }
                }
            }
            "--since" => options.since = Some(value.clone()),
            "--repo" => options.repository = Some(value.clone()),
            "--org" => options.organization = Some(value.clone()),
            _ => return Err(format!("unknown option for context: {arg}")),
        }
        index += 2;
    }
    let task = options.task();
    if task.chars().count() < 3 {
        return Err(
            "context needs the task, at least 3 characters: autter context \"<what you are about to do>\""
                .into(),
        );
    }
    if task.chars().count() > MAX_TASK_CHARS {
        return Err(format!(
            "the task must be at most {MAX_TASK_CHARS} characters"
        ));
    }
    if options.paths.len() > MAX_NAMES {
        return Err(format!("context takes at most {MAX_NAMES} --path values"));
    }
    if options.symbols.len() > MAX_NAMES {
        return Err(format!("context takes at most {MAX_NAMES} --symbol values"));
    }
    Ok(options)
}

/// The request body for `POST /context`; `paths` are already repo-relative.
fn request_body(repository: &str, options: &Options, paths: &[String]) -> Value {
    let mut body = Map::new();
    body.insert("repository".into(), json!(repository));
    body.insert("task".into(), json!(options.task()));
    if !paths.is_empty() {
        body.insert("paths".into(), json!(paths));
    }
    if !options.symbols.is_empty() {
        body.insert("symbols".into(), json!(options.symbols));
    }
    if let Some(budget) = options.budget {
        body.insert("budget_tokens".into(), json!(budget));
    }
    if !options.include.is_empty() {
        body.insert("include".into(), json!(options.include));
    }
    if let Some(since) = &options.since {
        body.insert("since".into(), json!(since));
    }
    Value::Object(body)
}

fn store_label(store: &str) -> String {
    match store {
        "memory" => "Memory".into(),
        "wiki" => "Wiki".into(),
        "code" => "Code".into(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => "Context".into(),
            }
        }
    }
}

/// Text on one line, without terminal control sequences.
fn one_line(text: &str) -> String {
    clean(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `more:` for an item: the MCP tool and its arguments, and the CLI command
/// that reads the same thing when there is one.
fn read_more_line(read_more: &Value) -> Option<String> {
    let tool = read_more["tool"].as_str()?;
    let args = read_more.get("args").cloned().unwrap_or(json!({}));
    let mut line = format!("more: {} {}", one_line(tool), one_line(&args.to_string()));
    if tool == "get_repo_memory"
        && let Some(slug) = args["slug"].as_str()
    {
        line.push_str(&format!(" (CLI: autter memory show {})", one_line(slug)));
    }
    Some(line)
}

fn plural(count: u64, one: &str) -> String {
    format!("{count} {one}{}", if count == 1 { "" } else { "s" })
}

/// The context answer as Markdown for an agent to read.
fn render_context(result: &Value) -> String {
    let repo = one_line(result["repo"].as_str().unwrap_or("this repository"));
    let empty = Vec::new();
    let items = result["items"].as_array().unwrap_or(&empty);
    let mut out = format!("# Autter context for {repo}\n\n");
    let areas: Vec<String> = result["areas"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|area| {
            let name = area["name"].as_str().or_else(|| area["slug"].as_str())?;
            Some(match area["path"].as_str() {
                Some("") => format!("{} (whole repository)", one_line(name)),
                Some(path) => format!("{} (`{}`)", one_line(name), one_line(path)),
                None => one_line(name),
            })
        })
        .collect();
    if !areas.is_empty() {
        out.push_str(&format!("Areas: {}\n\n", areas.join(", ")));
    }
    let unchanged = result["unchanged"].as_u64().unwrap_or(0);
    if items.is_empty() {
        if unchanged > 0 {
            out.push_str("Nothing new: everything that matches is unchanged since --since.\n");
        } else {
            out.push_str(&format!(
                "Nothing in {repo}'s memory, wiki or code matches this task.\n"
            ));
        }
    }
    for item in items {
        let title = excerpt(item["title"].as_str().unwrap_or("(untitled)"), 160);
        let mut line = format!(
            "- **{} · {title}**",
            store_label(item["store"].as_str().unwrap_or(""))
        );
        if let Some(path) = item["path"].as_str().map(one_line)
            && !path.is_empty()
            && path != title
        {
            line.push_str(&format!(" ({path})"));
        }
        if let Some(id) = item["id"].as_str()
            && id.starts_with("rmf_")
        {
            line.push_str(&format!(" `{}`", one_line(id)));
        }
        let text = one_line(item["text"].as_str().unwrap_or(""));
        if !text.is_empty() {
            line.push_str(&format!(": {text}"));
        }
        out.push_str(&line);
        out.push('\n');
        if let Some(more) = read_more_line(&item["read_more"]) {
            out.push_str(&format!("  {more}\n"));
        }
    }
    let mut notes = Vec::new();
    let omitted: Vec<String> = STORES
        .iter()
        .filter_map(|store| {
            let count = result["omitted"][*store].as_u64().filter(|n| *n > 0)?;
            Some(format!(
                "{count} {store} {}",
                if count == 1 { "item" } else { "items" }
            ))
        })
        .collect();
    if !omitted.is_empty() {
        let budget = result["tokens"]["budget"]
            .as_u64()
            .map(|budget| format!(" to stay within {budget} tokens"))
            .unwrap_or_default();
        notes.push(format!(
            "Left out{budget}: {}. Raise --budget (up to 16000) or narrow with --path or --include.",
            omitted.join(", ")
        ));
    }
    if unchanged > 0 && !items.is_empty() {
        notes.push(format!(
            "{} you already have (unchanged since --since) left out.",
            plural(unchanged, "item")
        ));
    }
    if result["sources"]["indexed"] == false {
        notes.push(
            "The code index for this repository is not ready, so code structure is missing.".into(),
        );
    }
    if !notes.is_empty() {
        out.push('\n');
        for note in notes {
            out.push_str(&note);
            out.push('\n');
        }
    }
    if let Some(cursor) = result["cursor"].as_str().filter(|c| !c.is_empty()) {
        out.push_str(&format!("\ncursor: {}\n", one_line(cursor)));
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
    // Named files, else the files changed in this checkout (none is fine).
    let paths = resolve_files(&options.paths, files_repo)?;
    let (context, slug) = org_api_context(options.organization.as_deref(), 60)?;
    let result = response_json_or(
        context.post_json(
            &format!("/api/cli/orgs/{}/context", component(&slug)),
            &request_body(&repository, &options, &paths),
        )?,
        MISSING_ROUTE,
    )?;
    if arg_parser::json() {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print!("{}", render_context(&result));
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
        arg_parser::print_command_help("context");
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
        arg_parser::print_command_help("context");
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
    fn options_take_a_task_and_every_filter() {
        let options = parse_options(&args(&[
            "add",
            "retries",
            "to",
            "the",
            "webhook",
            "--path",
            "src/billing/webhooks.ts",
            "--path",
            "src/http/",
            "--symbol",
            "handleStripeEvent",
            "--budget",
            "6000",
            "--include",
            "memory,code",
            "--include",
            "code",
            "--since",
            "c1.abc",
            "--repo",
            "acme/api",
            "--org",
            "acme",
        ]))
        .unwrap();
        assert_eq!(options.task(), "add retries to the webhook");
        assert_eq!(options.paths, vec!["src/billing/webhooks.ts", "src/http/"]);
        assert_eq!(options.symbols, vec!["handleStripeEvent"]);
        assert_eq!(options.budget, Some(6000));
        assert_eq!(options.include, vec!["memory", "code"]);
        assert_eq!(options.since.as_deref(), Some("c1.abc"));
        assert_eq!(options.repository.as_deref(), Some("acme/api"));
        assert_eq!(options.organization.as_deref(), Some("acme"));
        // A quoted task is one argument.
        let quoted = parse_options(&args(&["fix the retry budget"])).unwrap();
        assert_eq!(quoted.task(), "fix the retry budget");
    }

    #[test]
    fn options_are_validated_before_anything_is_sent() {
        assert!(parse_options(&args(&[])).is_err());
        assert!(parse_options(&args(&["ab"])).is_err());
        assert!(parse_options(&args(&["fix it", "--budget", "299"])).is_err());
        assert!(parse_options(&args(&["fix it", "--budget", "16001"])).is_err());
        assert!(parse_options(&args(&["fix it", "--budget", "lots"])).is_err());
        assert!(parse_options(&args(&["fix it", "--budget", "300"])).is_ok());
        assert!(parse_options(&args(&["fix it", "--budget", "16000"])).is_ok());
        assert!(parse_options(&args(&["fix it", "--include", "secrets"])).is_err());
        assert!(parse_options(&args(&["fix it", "--symbol", " "])).is_err());
        assert!(parse_options(&args(&["fix it", "--path"])).is_err());
        assert!(parse_options(&args(&["fix it", "--kind", "code"])).is_err());
        assert!(parse_options(&["x".repeat(2_001)]).is_err());
        let mut many = args(&["fix it"]);
        for i in 0..21 {
            many.push("--path".into());
            many.push(format!("f{i}"));
        }
        assert!(parse_options(&many).is_err());
    }

    #[test]
    fn the_request_carries_only_what_was_asked() {
        let options = parse_options(&args(&["fix the retry budget"])).unwrap();
        assert_eq!(
            request_body("https://github.com/acme/api", &options, &[]),
            json!({ "repository": "https://github.com/acme/api", "task": "fix the retry budget" })
        );
        let options = parse_options(&args(&[
            "fix the retry budget",
            "--symbol",
            "retry",
            "--budget",
            "4000",
            "--include",
            "memory,wiki,code",
            "--since",
            "cur",
        ]))
        .unwrap();
        assert_eq!(
            request_body(
                "https://github.com/acme/api",
                &options,
                &["src/http/retry.ts".into()]
            ),
            json!({
                "repository": "https://github.com/acme/api",
                "task": "fix the retry budget",
                "paths": ["src/http/retry.ts"],
                "symbols": ["retry"],
                "budget_tokens": 4000,
                "include": ["memory", "wiki", "code"],
                "since": "cur"
            })
        );
    }

    #[test]
    fn context_renders_as_markdown_with_fact_ids_and_pointers() {
        let result = json!({
            "repo": "acme/api",
            "task": "add retries",
            "areas": [
                { "slug": "src-billing", "name": "Billing", "path": "src/billing/" },
                { "slug": "root", "name": "Root", "path": "" }
            ],
            "items": [
                {
                    "store": "memory", "id": "rmf_ab12cd34ef", "kind": "fact",
                    "title": "Stripe webhooks arrive twice",
                    "text": "Dedupe on the event id;\nnever on\u{1b}[31m the payload.",
                    "path": "src/billing/webhooks.ts", "area": "src-billing",
                    "score": 0.93, "tokens": 42,
                    "read_more": { "tool": "get_repo_memory", "args": { "slug": "areas/src-billing" } }
                },
                {
                    "store": "wiki", "id": "wiki:/modules/billing", "kind": "module",
                    "title": "Billing module", "text": "Charges and refunds.",
                    "path": null, "score": 0.7, "tokens": 20, "read_more": null
                },
                {
                    "store": "code", "id": "file:src/http/retry.ts", "kind": "file",
                    "title": "src/http/retry.ts", "text": "exports retryWithBackoff",
                    "path": "src/http/retry.ts", "score": 0.6, "tokens": 12,
                    "read_more": { "tool": "get_file_context", "args": { "paths": ["src/http/retry.ts"] } }
                }
            ],
            "omitted": { "memory": 0, "wiki": 0, "code": 2 },
            "unchanged": 1,
            "tokens": { "served": 74, "budget": 4000 },
            "cursor": "c1.opaque",
            "sources": { "search": "hybrid", "memory_facts": 12, "wiki_pages": 2, "indexed": true },
            "cached": false
        });
        assert_eq!(
            render_context(&result),
            "# Autter context for acme/api\n\
             \nAreas: Billing (`src/billing/`), Root (whole repository)\n\
             \n- **Memory · Stripe webhooks arrive twice** (src/billing/webhooks.ts) `rmf_ab12cd34ef`: Dedupe on the event id; never on[31m the payload.\
             \n  more: get_repo_memory {\"slug\":\"areas/src-billing\"} (CLI: autter memory show areas/src-billing)\
             \n- **Wiki · Billing module**: Charges and refunds.\
             \n- **Code · src/http/retry.ts**: exports retryWithBackoff\
             \n  more: get_file_context {\"paths\":[\"src/http/retry.ts\"]}\n\
             \nLeft out to stay within 4000 tokens: 2 code items. Raise --budget (up to 16000) or narrow with --path or --include.\
             \n1 item you already have (unchanged since --since) left out.\n\
             \ncursor: c1.opaque\n"
        );
    }

    #[test]
    fn empty_and_unindexed_answers_say_so() {
        let result = json!({
            "repo": "acme/api", "areas": [], "items": [],
            "omitted": { "memory": 0, "wiki": 0, "code": 0 }, "unchanged": 0,
            "tokens": { "served": 0, "budget": 4000 }, "cursor": "c2",
            "sources": { "indexed": false }
        });
        assert_eq!(
            render_context(&result),
            "# Autter context for acme/api\n\
             \nNothing in acme/api's memory, wiki or code matches this task.\n\
             \nThe code index for this repository is not ready, so code structure is missing.\n\
             \ncursor: c2\n"
        );
        let unchanged = json!({ "repo": "acme/api", "items": [], "unchanged": 3, "cursor": "c3" });
        assert!(render_context(&unchanged).contains("Nothing new"));
    }
}
