use crate::git::repository::Repository;

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn truncate_subject(subject: &str, max_len: usize) -> String {
    if subject.chars().count() <= max_len {
        return subject.to_string();
    }
    let mut end = 0usize;
    for (idx, _) in subject.char_indices() {
        if idx >= max_len.saturating_sub(3) {
            break;
        }
        end = idx;
    }
    format!("{}...", &subject[..=end])
}

/// Best-effort description of HEAD for usage examples, e.g. `abc1234 — Fix login`.
pub fn head_commit_hint(repo: &Repository) -> Option<String> {
    let head = repo.head().ok()?;
    let sha = head.target().ok()?;
    let commit = repo.revparse_single("HEAD").ok()?.peel_to_commit().ok()?;
    let summary = commit.summary().ok()?;
    let short = short_sha(&sha);
    if summary.is_empty() {
        Some(short)
    } else {
        Some(format!("{short}: {}", truncate_subject(&summary, 50)))
    }
}

fn sample_tracked_file(repo: &Repository) -> Option<String> {
    let mut args = repo.global_args_for_exec();
    args.push("ls-tree".to_string());
    args.push("-r".to_string());
    args.push("--name-only".to_string());
    args.push("HEAD".to_string());
    let output = crate::git::repository::exec_git(&args).ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout
        .lines()
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

pub fn diff_args_missing_commit(args: &[String]) -> bool {
    let mut positional = 0usize;
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--blame-deletions-since" => {
                if i + 1 < args.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            arg if arg.starts_with("--") => i += 1,
            _ => {
                positional += 1;
                i += 1;
            }
        }
    }
    positional == 0
}

pub fn eprint_missing_diff_args(repo: &Repository) {
    eprintln!("Error: diff requires a commit or commit range argument");
    eprintln!();
    eprintln!("Try:");
    if let Some(hint) = head_commit_hint(repo) {
        eprintln!("  autter diff HEAD              # latest commit ({hint})");
    } else {
        eprintln!("  autter diff HEAD              # latest commit");
    }
    eprintln!("  autter diff HEAD~1..HEAD      # changes in the latest commit");
    eprintln!();
    eprintln!("Usage: autter diff <commit>");
    eprintln!("       autter diff <commit1>..<commit2>");
}

pub fn eprint_missing_show_args(repo: &Repository) {
    eprintln!("Error: show requires a revision or range");
    eprintln!();
    eprintln!("Try:");
    if let Some(hint) = head_commit_hint(repo) {
        eprintln!("  autter show HEAD              # latest commit ({hint})");
    } else {
        eprintln!("  autter show HEAD              # latest commit");
    }
    eprintln!("  autter show HEAD~5..HEAD      # last five commits");
    eprintln!();
    eprintln!("Usage: autter show <rev|range>");
}

pub fn eprint_missing_blame_args(repo: &Repository) {
    eprintln!("Error: blame requires a file argument");
    eprintln!();
    eprintln!("Try:");
    if let Some(file) = sample_tracked_file(repo) {
        eprintln!("  autter blame {file}");
    } else {
        eprintln!("  autter blame README.md");
    }
    eprintln!("  autter blame -L 10,20 src/main.rs");
    eprintln!();
    eprintln!("Usage: autter blame <file>");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_args_missing_commit_detects_flags_without_revision() {
        assert!(diff_args_missing_commit(&[]));
        assert!(diff_args_missing_commit(&["--json".to_string()]));
        assert!(diff_args_missing_commit(&[
            "--blame-deletions".to_string(),
            "--include-stats".to_string(),
        ]));
        assert!(!diff_args_missing_commit(&["HEAD".to_string()]));
        assert!(!diff_args_missing_commit(&[
            "--json".to_string(),
            "HEAD".to_string(),
        ]));
    }

    #[test]
    fn truncate_subject_shortens_long_messages() {
        assert_eq!(truncate_subject("short", 50), "short");
        assert_eq!(
            truncate_subject(
                "this is a very long commit subject that should be shortened",
                20
            ),
            "this is a very lo..."
        );
    }
}
