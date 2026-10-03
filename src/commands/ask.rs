//! System investigations, captured log queries, and shared Dashboard thread history.
use crate::api::client::ApiContext;
use crate::auth::identity::extract_identity_from_access_token;
use crate::commands::arg_parser::{self, ScanMode};
use crate::error::AutterError;
use serde_json::{Value, json};

#[derive(Default, Debug)]
struct Options {
    repository: Option<String>,
    organization: Option<String>,
    mode: Option<String>,
    thread: Option<String>,
    words: Vec<String>,
    query: serde_json::Map<String, Value>,
    filters: Vec<Value>,
    offset: usize,
}

fn parse_options(command: &str, args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if flag == "--" {
            options.words.extend_from_slice(&args[index + 1..]);
            break;
        }
        if !flag.starts_with('-') {
            options.words.push(args[index].clone());
            index += 1;
            continue;
        }
        let allowed = matches!(flag, "--repo" | "--org")
            || (command == "ask" && matches!(flag, "--thread" | "--mode"))
            || (command == "threads" && flag == "--offset")
            || (command == "logs"
                && matches!(
                    flag,
                    "--severity"
                        | "--service"
                        | "--environment"
                        | "--from"
                        | "--to"
                        | "--since"
                        | "--query"
                        | "--limit"
                        | "--field"
                        | "--filter"
                ));
        if !allowed {
            return Err(format!("unknown option for {command}: {flag}"));
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        if value.starts_with("--") {
            return Err(format!("{flag} requires a value"));
        }
        match flag {
            "--repo" => options.repository = Some(value.clone()),
            "--org" => options.organization = Some(value.clone()),
            "--thread" => options.thread = Some(value.clone()),
            "--mode" => {
                if !matches!(value.as_str(), "system" | "codebase" | "reviews") {
                    return Err("--mode must be system, codebase, or reviews".into());
                }
                options.mode = Some(value.clone());
            }
            "--offset" => {
                options.offset = value
                    .parse()
                    .map_err(|_| "--offset must be a nonnegative integer")?
            }
            "--limit" => {
                let limit: u64 = value
                    .parse()
                    .map_err(|_| "--limit must be between 1 and 200")?;
                if !(1..=200).contains(&limit) {
                    return Err("--limit must be between 1 and 200".into());
                }
                options.query.insert("limit".into(), json!(limit));
            }
            "--since" => {
                let unit = ["m", "h", "d"]
                    .into_iter()
                    .find(|unit| value.ends_with(unit))
                    .ok_or("--since accepts m, h, or d")?;
                let amount = value.strip_suffix(unit).unwrap();
                let amount: i64 = amount
                    .parse()
                    .map_err(|_| "--since accepts a duration such as 30m, 1h, or 7d")?;
                let multiplier = match unit {
                    "m" => 60,
                    "h" => 3600,
                    "d" => 86400,
                    _ => return Err("--since accepts m, h, or d".into()),
                };
                let seconds = amount
                    .checked_mul(multiplier)
                    .filter(|seconds| *seconds > 0 && *seconds <= 90 * 86400)
                    .ok_or("--since must be positive and at most 90d")?;
                options.query.insert(
                    "from".into(),
                    json!((chrono::Utc::now() - chrono::Duration::seconds(seconds)).to_rfc3339()),
                );
            }
            "--field" => {
                let (field, value) = value
                    .split_once('=')
                    .ok_or("--field uses field=value (custom fields: attributes.<name>)")?;
                if field.is_empty() {
                    return Err("--field requires a field name".into());
                }
                options
                    .filters
                    .push(json!({ "field": field, "operator": "eq", "value": value }));
            }
            "--filter" => {
                let parts: Vec<_> = value.splitn(3, ':').collect();
                if parts.len() < 2
                    || parts[0].is_empty()
                    || !matches!(
                        parts[1],
                        "eq" | "neq" | "contains" | "gt" | "gte" | "lt" | "lte" | "exists"
                    )
                    || (parts.len() < 3 && parts[1] != "exists")
                {
                    return Err("--filter uses field:operator:value; operators: eq, neq, contains, gt, gte, lt, lte, exists".into());
                }
                options.filters.push(json!({ "field": parts[0], "operator": parts[1], "value": parts.get(2).copied().unwrap_or("") }));
            }
            _ => {
                options
                    .query
                    .insert(flag.trim_start_matches('-').into(), json!(value));
            }
        }
        index += 2;
    }
    Ok(options)
}

fn component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn response_json(response: crate::http::Response) -> Result<Value, AutterError> {
    let code = response.status_code;
    let text = response
        .as_str()
        .map_err(|error| AutterError::Generic(error.to_string()))?;
    let value: Value = serde_json::from_str(text).map_err(|_| AutterError::Generic(format!("Ask API returned HTTP {code}. Update the API deployment if these commands are unavailable.")))?;
    if code != 200 {
        return Err(AutterError::Generic(format!(
            "HTTP {code}: {}",
            value["error"].as_str().unwrap_or("Ask request failed")
        )));
    }
    Ok(value)
}

fn clean(value: &Value) -> String {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn print_table(columns: &[String], rows: &[Vec<Value>]) {
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            rows.iter()
                .filter_map(|row| row.get(index))
                .map(|value| clean(value).chars().count())
                .max()
                .unwrap_or(0)
                .max(column.len())
                .min(if column == "message" || column == "title" {
                    72
                } else {
                    36
                })
        })
        .collect();
    for (index, column) in columns.iter().enumerate() {
        print!("{:<width$}  ", column, width = widths[index]);
    }
    println!();
    for width in &widths {
        print!("{}  ", "─".repeat(*width));
    }
    println!();
    for row in rows {
        for (index, width) in widths.iter().enumerate() {
            let text = row.get(index).map(clean).unwrap_or_default();
            let text = if text.chars().count() > *width {
                format!(
                    "{}…",
                    text.chars()
                        .take(width.saturating_sub(1))
                        .collect::<String>()
                )
            } else {
                text
            };
            print!("{:<width$}  ", text, width = width);
        }
        println!();
    }
}

fn run(command: &str, mut options: Options) -> Result<(), AutterError> {
    let mut context = ApiContext::new(None).with_timeout(210);
    let token = context
        .auth_token
        .as_ref()
        .ok_or_else(|| AutterError::Generic("Sign in with `autter login` first.".into()))?;
    let identity = extract_identity_from_access_token(token);
    let selected = if let Some(slug) = options.organization.as_deref() {
        identity
            .orgs
            .iter()
            .find(|org| org.org_slug.as_deref() == Some(slug))
    } else {
        identity.active_org()
    }
    .ok_or_else(|| {
        AutterError::Generic("Choose an organization with --org <slug>, or sign in again.".into())
    })?;
    let slug = selected.org_slug.as_deref().ok_or_else(|| {
        AutterError::Generic("Organization slug unavailable. Sign in again.".into())
    })?;
    if selected.org_id != identity.active_org_id {
        let org_id = selected
            .org_id
            .as_deref()
            .ok_or_else(|| AutterError::Generic("Organization identity unavailable.".into()))?;
        context.auth_token = Some(crate::api::client::access_token_for_org(org_id).ok_or_else(
            || AutterError::Generic("Could not authenticate for this organization.".into()),
        )?);
    }
    let base = format!("/api/cli/orgs/{}/ask", component(slug));
    let result = match command {
        "ask" => {
            if let Some(id) = options.thread.as_deref() {
                let thread =
                    response_json(context.get(&format!("{base}/threads/{}", component(id)))?)?;
                if options.repository.is_none() {
                    options.repository = thread["repository"].as_str().map(str::to_owned);
                }
                if options.mode.is_none() {
                    options.mode = thread["mode"].as_str().map(str::to_owned);
                }
            }
            response_json(context.post_json(&format!("{base}/chat"), &json!({ "question": options.words.join(" "), "repository": options.repository, "mode": options.mode.unwrap_or_else(|| "system".into()), "threadId": options.thread, "format": "json" }))?)?
        }
        "logs" => {
            options
                .query
                .insert("repository".into(), json!(options.repository));
            options
                .query
                .insert("filters".into(), json!(options.filters));
            response_json(context.post_json(&format!("{base}/logs"), &options.query)?)?
        }
        "threads" if options.words.first().is_some_and(|word| word == "show") => response_json(
            context.get(&format!("{base}/threads/{}", component(&options.words[1])))?,
        )?,
        "threads" => {
            let query = options
                .words
                .iter()
                .skip(usize::from(
                    options.words.first().is_some_and(|word| word == "search"),
                ))
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            let mut params = url::form_urlencoded::Serializer::new(String::new());
            params
                .append_pair("q", &query)
                .append_pair("offset", &options.offset.to_string());
            if let Some(repo) = options.repository {
                params.append_pair("repository", &repo);
            }
            response_json(context.get(&format!("{base}/threads?{}", params.finish()))?)?
        }
        _ => unreachable!(),
    };
    if arg_parser::json() {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    match command {
        "ask" => {
            // Preserve newlines in prose while stripping terminal control sequences.
            println!(
                "{}",
                result["answer"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                    .collect::<String>()
            );
            if !arg_parser::quiet() {
                println!("\nThread: {}", result["threadId"].as_str().unwrap_or(""));
            }
        }
        "logs" => {
            let columns: Vec<String> = serde_json::from_value(result["columns"].clone())?;
            let rows: Vec<Vec<Value>> = serde_json::from_value(result["rows"].clone())?;
            let selected: Vec<usize> = columns
                .iter()
                .enumerate()
                .filter_map(|(index, name)| {
                    matches!(
                        name.as_str(),
                        "timestamp" | "severity" | "service" | "environment" | "message"
                    )
                    .then_some(index)
                })
                .collect();
            print_table(
                &selected
                    .iter()
                    .map(|index| {
                        if columns[*index] == "timestamp" {
                            "timestamp (UTC)".to_string()
                        } else {
                            columns[*index].clone()
                        }
                    })
                    .collect::<Vec<_>>(),
                &rows
                    .iter()
                    .map(|row| selected.iter().map(|index| row[*index].clone()).collect())
                    .collect::<Vec<_>>(),
            );
            println!(
                "{} captured events{}",
                rows.len(),
                if result["truncated"] == true {
                    " (more match; narrow filters or time range)"
                } else {
                    ""
                }
            );
            if !arg_parser::quiet() {
                println!(
                    "Use --json for full messages, occurrence IDs, traces, and custom fields."
                );
            }
        }
        "threads" if result.get("messages").is_some() => {
            println!("{}\n", clean(&result["title"]));
            if let Some(messages) = result["messages"].as_array() {
                for message in messages {
                    println!("{}:", clean(&message["role"]));
                    if let Some(parts) = message["parts"].as_array() {
                        for part in parts {
                            if let Some(text) = part["text"].as_str() {
                                println!(
                                    "{}",
                                    text.chars()
                                        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                                        .collect::<String>()
                                );
                            }
                        }
                    }
                    println!();
                }
            }
        }
        "threads" => {
            let rows: Vec<Vec<Value>> = result["threads"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|thread| {
                    vec![
                        thread["id"].clone(),
                        thread["title"].clone(),
                        thread["repository"].clone(),
                        thread["updatedAt"].clone(),
                    ]
                })
                .collect();
            print_table(
                &[
                    "id".into(),
                    "title".into(),
                    "repository".into(),
                    "updated".into(),
                ],
                &rows,
            );
            if rows.is_empty() {
                println!("No saved threads match this search.");
            }
            if rows.len() == 50 {
                println!(
                    "More threads may match. Continue with --offset {}.",
                    options.offset + 50
                );
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

pub fn handle(command: &str, args: &[String]) {
    let parsed = arg_parser::pre_parse(args, ScanMode::Full, true).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        std::process::exit(2);
    });
    arg_parser::merge_global_flags(&parsed.flags);
    if parsed.flags.help {
        arg_parser::print_command_help(command);
        return;
    }
    if let Some(directory) = parsed.flags.change_dir
        && let Err(error) = std::env::set_current_dir(directory)
    {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
    let options = parse_options(command, &parsed.rest).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        std::process::exit(2);
    });
    if (command == "ask" && options.words.join(" ").trim().is_empty())
        || (command == "logs" && !options.words.is_empty())
        || (command == "threads"
            && options.words.first().is_some_and(|word| word == "show")
            && options.words.len() != 2)
    {
        arg_parser::print_command_help(command);
        std::process::exit(2);
    }
    if let Err(error) = run(command, options) {
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
    fn ask_options_support_resume_and_scope() {
        let options = parse_options(
            "ask",
            &args(&[
                "Why did checkout fail?",
                "--repo",
                "shop",
                "--thread",
                "thread-id",
            ]),
        )
        .unwrap();
        assert_eq!(options.words, vec!["Why did checkout fail?"]);
        assert_eq!(options.thread.as_deref(), Some("thread-id"));
        assert!(parse_options("ask", &args(&["--mode", "invalid"])).is_err());
        assert!(parse_options("ask", &args(&["--severity", "error"])).is_err());
    }

    #[test]
    fn log_options_preserve_fields_and_validate_bounds() {
        let options = parse_options(
            "logs",
            &args(&[
                "--severity",
                "error",
                "--since",
                "1h",
                "--field",
                "attributes.route=/checkout?a=b",
                "--filter",
                "status_code:gte:500",
            ]),
        )
        .unwrap();
        assert_eq!(options.filters[0]["value"], "/checkout?a=b");
        assert_eq!(options.filters[1]["operator"], "gte");
        assert!(parse_options("logs", &args(&["--limit", "201"])).is_err());
        assert!(parse_options("logs", &args(&["--since", "0h"])).is_err());
        assert!(parse_options("logs", &args(&["--since", "1秒"])).is_err());
        assert!(parse_options("logs", &args(&["--filter", "x:sql:drop"])).is_err());
        assert!(parse_options("logs", &args(&["--service", "--severity", "error"])).is_err());
    }

    #[test]
    fn terminal_output_removes_control_characters() {
        assert_eq!(clean(&json!("hello\u{1b}[31m\nworld")), "hello [31m world");
    }
}
