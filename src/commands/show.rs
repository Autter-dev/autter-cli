use crate::authorship::guidance;
use crate::authorship::record_status::{DEFAULT_PENDING_WAIT, RecordClassifier};
use crate::error::AutterError;
use crate::git::find_repository;
use crate::git::notes_api::{
    CommitAuthorship, filter_commits_with_notes as get_commits_with_notes_from_list,
};
use crate::git::repository::{CommitRange, Repository};
use serde::Serialize;
use std::time::Duration;

pub fn handle_show(args: &[String]) {
    use crate::commands::output::{self, ErrorKind};

    // Record `--json` before reporting any bad argument, so that error is JSON too.
    if args.iter().any(|arg| arg == "--json") {
        crate::commands::arg_parser::merge_global_flags(
            &crate::commands::arg_parser::GlobalFlags {
                json: true,
                ..Default::default()
            },
        );
    }
    let mut revisions = Vec::new();
    for arg in args {
        if arg == "--json" {
            continue;
        } else if arg.starts_with('-') {
            // A revision never starts with `-`: an option here is a typo, not a ref.
            crate::commands::arg_parser::exit_unknown_option("show", arg);
        } else {
            revisions.push(arg.clone());
        }
    }
    let repo = match find_repository(&Vec::<String>::new()) {
        Ok(repo) => repo,
        Err(e) => crate::commands::exit_repository_error("show", &e),
    };

    if revisions.is_empty() {
        crate::commands::usage_hints::eprint_missing_show_args(&repo);
        output::fail(ErrorKind::Usage, "show requires a revision or range");
    }

    if revisions.len() > 1 {
        output::fail(
            ErrorKind::Usage,
            "Error: show accepts exactly one revision or range",
        );
    }

    let result = if crate::commands::arg_parser::json() {
        show_authorship_json(&repo, &revisions[0])
    } else {
        show_authorship(&repo, &revisions[0])
    };
    if let Err(e) = result {
        let kind = match e {
            AutterError::GitCliError { .. } => ErrorKind::RevisionNotFound,
            _ => ErrorKind::Runtime,
        };
        output::fail(kind, format!("Failed to show authorship: {}", e));
    }
}

/// `autter show <rev|range> --json`: one entry per commit, newest first as
/// git lists them. `authorship` is null when the commit has no note.
#[derive(Serialize)]
struct ShowReport {
    commits: Vec<ShowCommit>,
}

#[derive(Serialize)]
struct ShowCommit {
    commit: String,
    note_present: bool,
    authorship: Option<ShowAuthorship>,
}

#[derive(Serialize)]
struct ShowAuthorship {
    /// Per file, the attested line ranges. `kind` is `ai` for an AI session
    /// or prompt and `human` for a known human (`h_` hashes); lines with no
    /// entry are untracked.
    attestations: Vec<ShowFileAttestation>,
    /// The note's metadata section as stored (prompts, humans, sessions).
    metadata: crate::authorship::authorship_log_serialization::AuthorshipMetadata,
}

#[derive(Serialize)]
struct ShowFileAttestation {
    file: String,
    entries: Vec<ShowAttestationEntry>,
}

#[derive(Serialize)]
struct ShowAttestationEntry {
    hash: String,
    kind: &'static str,
    /// Inclusive `[start, end]` line ranges.
    lines: Vec<[u32; 2]>,
}

fn show_authorship_json(repo: &Repository, spec: &str) -> Result<(), AutterError> {
    use crate::authorship::authorship_log::LineRange;

    let commits = resolve_commits(repo, spec)?;
    let entries = if commits.is_empty() {
        Vec::new()
    } else {
        get_commits_with_notes_from_list(repo, &commits)?
    };
    let commits = entries
        .into_iter()
        .map(|entry| match entry {
            CommitAuthorship::Log {
                sha,
                authorship_log,
                ..
            } => ShowCommit {
                commit: sha,
                note_present: true,
                authorship: Some(ShowAuthorship {
                    attestations: authorship_log
                        .attestations
                        .iter()
                        .map(|file| ShowFileAttestation {
                            file: file.file_path.clone(),
                            entries: file
                                .entries
                                .iter()
                                .map(|entry| ShowAttestationEntry {
                                    hash: entry.hash.clone(),
                                    kind: if entry.hash.starts_with("h_") {
                                        "human"
                                    } else {
                                        "ai"
                                    },
                                    lines: entry
                                        .line_ranges
                                        .iter()
                                        .map(|range| match range {
                                            LineRange::Single(line) => [*line, *line],
                                            LineRange::Range(start, end) => [*start, *end],
                                        })
                                        .collect(),
                                })
                                .collect(),
                        })
                        .collect(),
                    metadata: authorship_log.metadata,
                }),
            },
            CommitAuthorship::NoLog { sha, .. } => ShowCommit {
                commit: sha,
                note_present: false,
                authorship: None,
            },
        })
        .collect();
    crate::commands::output::print_json(&ShowReport { commits });
    Ok(())
}

fn show_authorship(repo: &Repository, spec: &str) -> Result<(), AutterError> {
    let commits = resolve_commits(repo, spec)?;
    if commits.is_empty() {
        println!("No commits in {spec}.");
        return Ok(());
    }

    let entries = get_commits_with_notes_from_list(repo, &commits)?;
    let classifier = RecordClassifier::new(repo);
    // Only a single commit is worth waiting for: it is usually HEAD, right
    // after `git commit`, while the background service writes its note.
    let wait = if entries.len() == 1 {
        DEFAULT_PENDING_WAIT
    } else {
        Duration::ZERO
    };

    let multiple_commits = entries.len() > 1;
    for (index, entry) in entries.iter().enumerate() {
        if multiple_commits && index > 0 {
            println!();
        }

        match entry {
            CommitAuthorship::Log {
                sha,
                authorship_log,
                ..
            } => {
                if multiple_commits {
                    println!("{}", sha);
                }
                let serialized = authorship_log.serialize_to_string().map_err(|_| {
                    AutterError::Generic("Failed to serialize authorship log".to_string())
                })?;
                println!("{}", serialized);
            }
            CommitAuthorship::NoLog { sha, .. } => {
                if multiple_commits {
                    println!("{}", sha);
                }
                let record = classifier.classify(sha, wait);
                if record.is_recorded()
                    && let Some(note) = crate::git::notes_api::read_note(repo, sha)
                {
                    println!("{}", note.trim_end());
                    continue;
                }
                print!("{}", guidance::show_missing_record_message(&record));
            }
        }
    }

    Ok(())
}

fn resolve_commits(repo: &Repository, spec: &str) -> Result<Vec<String>, AutterError> {
    if let Some((start, end)) = spec.split_once("..") {
        if start.is_empty() || end.is_empty() {
            return Err(AutterError::Generic(
                "Invalid commit range format. Expected <start>..<end>".to_string(),
            ));
        }

        let range = CommitRange::new_infer_refname(repo, start.to_string(), end.to_string(), None)?;

        let mut commits: Vec<String> = range.into_iter().map(|commit| commit.id()).collect();

        if commits.is_empty() {
            let end_commit = repo.revparse_single(end)?;
            commits.push(end_commit.id());
        }

        Ok(commits)
    } else {
        let commit = repo.revparse_single(spec)?;
        Ok(vec![commit.id()])
    }
}
