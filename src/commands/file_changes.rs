//! `autter file-changes` — show the most frequently changed files in the repo.

use crate::error::AutterError;
use crate::file_changes::{resolve_repo_key, top_changed_files};
use crate::git::find_repository;

const DEFAULT_LIMIT: usize = 20;

pub fn handle_file_changes(args: &[String]) {
    let mut json_output = false;
    let mut limit = DEFAULT_LIMIT;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json_output = true,
            "--limit" | "-n" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --limit requires a value");
                    std::process::exit(1);
                }
                limit = match args[i].parse::<usize>() {
                    Ok(n) if n > 0 => n,
                    _ => {
                        eprintln!("error: --limit must be a positive integer");
                        std::process::exit(1);
                    }
                };
            }
            other => {
                eprintln!("error: unknown argument: {}", other);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let repo = match find_repository(&Vec::<String>::new()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    match run(&repo, limit, json_output) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}

fn run(
    repo: &crate::git::repository::Repository,
    limit: usize,
    json_output: bool,
) -> Result<(), AutterError> {
    let repo_key = resolve_repo_key(repo);
    let rows = top_changed_files(&repo_key, limit)?;

    if json_output {
        let payload = serde_json::json!({
            "repo": repo_key,
            "files": rows.iter().map(|row| serde_json::json!({
                "path": row.file_path,
                "change_count": row.change_count,
                "lines_added": row.lines_added,
                "lines_deleted": row.lines_deleted,
                "last_changed_at": row.last_changed_at,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if rows.is_empty() {
        eprintln!("No file change history recorded for this repository yet.");
        eprintln!("Counts are tracked as you checkpoint edits.");
        return Ok(());
    }

    eprintln!("Most changed files (repo: {})", repo_key);
    eprintln!();
    eprintln!("  {:>6}  {:>8}  {:>8}  file", "edits", "+lines", "-lines");
    for row in rows {
        eprintln!(
            "  {:>6}  {:>8}  {:>8}  {}",
            row.change_count, row.lines_added, row.lines_deleted, row.file_path
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn test_default_limit_is_positive() {
        assert!(DEFAULT_LIMIT > 0);
    }
}
