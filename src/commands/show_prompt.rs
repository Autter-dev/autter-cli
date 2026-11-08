use crate::authorship::cas_bridge::{self, TranscriptLookup};
use crate::authorship::prompt_utils::resolve_prompt;
use crate::git::find_repository;

/// Handle the `show-prompt` command
///
/// Usage: `autter show-prompt <prompt_id> [--commit <rev>] [--offset <n>]`
///
/// Prints, as JSON, the prompt record from the authorship note where the given
/// prompt ID is found, with its transcript when one can be read. The ID can be
/// a short reference from `autter blame`, a full session ID, a legacy prompt
/// hash, or the agent's own session ID (see `prompt_utils`).
/// By default returns from the most recent commit containing the prompt.
pub fn handle_show_prompt(args: &[String]) {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {}", e);
            eprintln!("Run `autter show-prompt --help` for where to find a prompt ID.");
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    };

    let repo = match find_repository(&Vec::<String>::new()) {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(1);
        }
    };

    match resolve_prompt(
        &repo,
        &parsed.prompt_id,
        parsed.commit.as_deref(),
        parsed.offset,
    ) {
        Ok(resolved) => {
            let prompt_record = &resolved.record;
            let mut prompt_json = serde_json::to_value(prompt_record).unwrap_or_else(|_| {
                serde_json::json!({
                    "agent_id": prompt_record.agent_id,
                    "human_author": prompt_record.human_author,
                    "messages_url": prompt_record.messages_url,
                })
            });

            let lookup = cas_bridge::lookup_transcript(prompt_record.messages_url.as_deref());
            let mut transcript = serde_json::json!({ "status": lookup.status() });
            if let Some(reason) = lookup.reason()
                && let Some(obj) = transcript.as_object_mut()
            {
                obj.insert("reason".to_string(), serde_json::Value::String(reason));
            }
            if let TranscriptLookup::Found(messages) = &lookup
                && let Ok(messages_json) = serde_json::to_value(messages)
                && let Some(obj) = prompt_json.as_object_mut()
            {
                obj.insert("messages".to_string(), messages_json);
            }

            let mut output = serde_json::json!({
                "commit": resolved.commit_sha,
                "prompt_id": resolved.prompt_id,
                "prompt": prompt_json,
                "transcript": transcript,
            });
            if resolved.prompt_id != parsed.prompt_id
                && let Some(obj) = output.as_object_mut()
            {
                obj.insert(
                    "query".to_string(),
                    serde_json::Value::String(parsed.prompt_id.clone()),
                );
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
            );
        }
        Err(crate::error::AutterError::Generic(msg)) => {
            eprintln!("Error: {}", msg);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

#[derive(Debug)]
pub struct ParsedArgs {
    pub prompt_id: String,
    pub commit: Option<String>,
    pub offset: usize,
}

pub fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut prompt_id: Option<String> = None;
    let mut commit: Option<String> = None;
    let mut offset: Option<usize> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if arg == "--commit" {
            if i + 1 >= args.len() {
                return Err("--commit requires a value".to_string());
            }
            i += 1;
            commit = Some(args[i].clone());
        } else if arg == "--offset" {
            if i + 1 >= args.len() {
                return Err("--offset requires a value".to_string());
            }
            i += 1;
            offset = Some(
                args[i]
                    .parse::<usize>()
                    .map_err(|_| "--offset must be a non-negative integer")?,
            );
        } else if arg.starts_with('-') {
            return Err(format!("Unknown option: {}", arg));
        } else {
            if prompt_id.is_some() {
                return Err("Only one prompt ID can be specified".to_string());
            }
            prompt_id = Some(arg.clone());
        }

        i += 1;
    }

    let prompt_id = prompt_id.ok_or("show-prompt requires a prompt ID")?;

    // Validate mutual exclusivity of --commit and --offset
    if commit.is_some() && offset.is_some() {
        return Err("--commit and --offset are mutually exclusive".to_string());
    }

    Ok(ParsedArgs {
        prompt_id,
        commit,
        offset: offset.unwrap_or(0),
    })
}
