//! Keep a line's attribution when it only gains a trailing newline.
//!
//! When a file's last line has no trailing newline and someone appends below it,
//! that line gets a `\n`. Git then reports the line as changed in the new commit
//! (plain `git blame` blames the new commit for it), so the new note would leave
//! it unattributed, or credit it to whoever made the append. The line's text did
//! not change, so this copies its attribution from the commit that really wrote it.

use crate::authorship::authorship_log::LineRange;
use crate::authorship::authorship_log_serialization::{AttestationEntry, AuthorshipLog};
use crate::git::repository::{Repository, exec_git};

/// For every file whose parent version ended without a newline, copy the attribution
/// of that last line into `authorship_log` if this commit kept its text and nothing
/// in this commit already attributes it. Best effort: failures leave the log as is.
pub fn carry_over_last_line_attributions(
    repo: &Repository,
    authorship_log: &mut AuthorshipLog,
    parent_sha: &str,
    commit_sha: &str,
) {
    if parent_sha == "initial" {
        return;
    }
    let Ok(added_lines) = repo.diff_added_lines(parent_sha, commit_sha, None) else {
        return;
    };

    for (file_path, lines) in added_lines {
        let Some(line_number) = unterminated_last_line(repo, &file_path, parent_sha, commit_sha)
        else {
            continue;
        };
        if !lines.contains(&line_number) || is_attested(authorship_log, &file_path, line_number) {
            continue;
        }
        let Some((origin_sha, origin_line)) =
            blame_origin(repo, parent_sha, &file_path, line_number)
        else {
            continue;
        };
        let Some(origin_log) = crate::git::refs::get_authorship(repo, &origin_sha) else {
            continue;
        };
        let Some(hash) = attestation_hash_for_line(&origin_log, &file_path, origin_line) else {
            continue;
        };

        copy_metadata_for_hash(&origin_log, authorship_log, &hash);
        authorship_log
            .get_or_create_file(&file_path)
            .add_entry(AttestationEntry::new(
                hash,
                vec![LineRange::Single(line_number)],
            ));
    }
}

/// The 1-based number of the parent's last line when the parent version of the file
/// ends without a newline and this commit keeps that line's text.
fn unterminated_last_line(
    repo: &Repository,
    file_path: &str,
    parent_sha: &str,
    commit_sha: &str,
) -> Option<u32> {
    let parent = repo.get_file_content(file_path, parent_sha).ok()?;
    if parent.is_empty() || parent.ends_with(b"\n") {
        return None;
    }
    let parent = String::from_utf8(parent).ok()?;
    let last_text = parent.lines().last()?;
    let line_number = u32::try_from(parent.lines().count()).ok()?;

    let current = String::from_utf8(repo.get_file_content(file_path, commit_sha).ok()?).ok()?;
    let current_text = current.lines().nth(line_number as usize - 1)?;
    (current_text.trim_end_matches('\r') == last_text.trim_end_matches('\r')).then_some(line_number)
}

fn is_attested(authorship_log: &AuthorshipLog, file_path: &str, line: u32) -> bool {
    attestation_hash_for_line(authorship_log, file_path, line).is_some()
}

fn attestation_hash_for_line(
    authorship_log: &AuthorshipLog,
    file_path: &str,
    line: u32,
) -> Option<String> {
    authorship_log
        .attestations
        .iter()
        .filter(|file| file.file_path == file_path)
        .flat_map(|file| &file.entries)
        .find(|entry| entry.line_ranges.iter().any(|range| range.contains(line)))
        .map(|entry| entry.hash.clone())
}

/// The commit and original line number that `git blame` gives for `line` at `rev`.
fn blame_origin(repo: &Repository, rev: &str, file_path: &str, line: u32) -> Option<(String, u32)> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "blame".to_string(),
        "--porcelain".to_string(),
        "-L".to_string(),
        format!("{line},{line}"),
        rev.to_string(),
        "--".to_string(),
        file_path.to_string(),
    ]);
    let output = exec_git(&args).ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut header = stdout.lines().next()?.split_whitespace();
    let sha = header.next()?.to_string();
    let origin_line = header.next()?.parse().ok()?;
    Some((sha, origin_line))
}

/// Copy the human, prompt and session records that `hash` refers to. Copied counters
/// are zeroed: the lines were already counted in the commit that wrote them.
fn copy_metadata_for_hash(from: &AuthorshipLog, to: &mut AuthorshipLog, hash: &str) {
    if let Some(human) = from.metadata.humans.get(hash) {
        to.metadata
            .humans
            .entry(hash.to_string())
            .or_insert_with(|| human.clone());
    }
    let session_key = hash.split("::").next().unwrap_or(hash);
    for key in [hash, session_key] {
        if let Some(prompt) = from.metadata.prompts.get(key) {
            to.metadata
                .prompts
                .entry(key.to_string())
                .or_insert_with(|| {
                    let mut prompt = prompt.clone();
                    prompt.total_additions = 0;
                    prompt.total_deletions = 0;
                    prompt.accepted_lines = 0;
                    prompt.overriden_lines = 0;
                    prompt
                });
        }
        if let Some(session) = from.metadata.sessions.get(key) {
            to.metadata
                .sessions
                .entry(key.to_string())
                .or_insert_with(|| {
                    let mut session = session.clone();
                    session.stats = None;
                    session
                });
        }
    }
}
