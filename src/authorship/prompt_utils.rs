//! Finding the prompt (AI session) behind a line, by the IDs users see.
//!
//! The path from a line to the prompt that wrote it:
//!
//! 1. `autter blame <file>` ends (on a terminal) with a legend of the prompts
//!    behind the AI lines, each with a short reference such as `s_3f9a2c1d`.
//!    `--show-prompt` puts the same reference inline on each AI line.
//! 2. `autter blame --why <file>:<line>` prints that reference with the agent,
//!    model, the agent's own session ID, the commit and the transcript.
//! 3. `autter show-prompt <ref>` prints the full prompt record as JSON.
//!
//! A prompt ID is one of:
//! - a session ID, `s_` + 14 hex (what new notes record);
//! - an attestation key, `s_<14 hex>::t_<14 hex>` (one checkpoint in a session);
//! - a legacy prompt hash, 16 hex (older notes);
//! - a unique prefix of any of those, at least [`MIN_PROMPT_REF_HEX`] hex
//!   characters long, the way git takes a short commit SHA;
//! - the agent's own session ID (`agent_id.id`, such as a Claude Code session).

use std::collections::{BTreeMap, HashMap};

use crate::authorship::authorship_log::PromptRecord;
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::error::AutterError;
use crate::git::notes_api::{read_authorship as get_authorship, search_notes as grep_ai_notes};
use crate::git::repository::Repository;

/// Shortest prefix (in hex characters after any `s_`) that `show-prompt`
/// accepts as a short reference.
pub const MIN_PROMPT_REF_HEX: usize = 4;

/// Hex characters a short reference shows by default (`s_3f9a2c1d`).
pub const SHORT_PROMPT_REF_HEX: usize = 8;

/// How many commits `show-prompt` reads when notes are not in git (the http
/// notes backend keeps them in a local store that `git grep` cannot search).
const NOTES_STORE_SCAN_LIMIT: usize = 5000;

/// The prompt or session key for an ID: an attestation key `s_x::t_y` names
/// the session `s_x`.
pub fn prompt_key(id: &str) -> &str {
    if id.starts_with("s_") {
        id.split("::").next().unwrap_or(id)
    } else {
        id
    }
}

/// The hex part of a prompt key (`s_` stripped).
fn hex_part(key: &str) -> &str {
    key.strip_prefix("s_").unwrap_or(key)
}

/// Short reference for a prompt ID with `hex_len` hex characters, keeping the
/// `s_` prefix of session IDs: `s_3f9a2c1d4e5f60::t_…` → `s_3f9a2c1d`.
pub fn short_prompt_ref_with_len(id: &str, hex_len: usize) -> String {
    let key = prompt_key(id);
    let (prefix, hex) = match key.strip_prefix("s_") {
        Some(rest) => ("s_", rest),
        None => ("", key),
    };
    let end = hex
        .char_indices()
        .nth(hex_len)
        .map(|(i, _)| i)
        .unwrap_or(hex.len());
    format!("{prefix}{}", &hex[..end])
}

/// Short reference for a prompt ID: `s_` + 8 hex, or 8 hex for legacy hashes.
pub fn short_prompt_ref(id: &str) -> String {
    short_prompt_ref_with_len(id, SHORT_PROMPT_REF_HEX)
}

/// Short references for a set of prompt IDs, lengthened where two would
/// collide, keyed by [`prompt_key`].
pub fn short_prompt_refs<'a>(ids: impl IntoIterator<Item = &'a str>) -> HashMap<String, String> {
    let mut keys: Vec<&str> = ids.into_iter().map(prompt_key).collect();
    keys.sort_unstable();
    keys.dedup();

    let mut out = HashMap::new();
    for key in &keys {
        let mut len = SHORT_PROMPT_REF_HEX;
        let max = hex_part(key).chars().count();
        while len < max {
            let short = short_prompt_ref_with_len(key, len);
            let collides = keys
                .iter()
                .any(|other| other != key && prompt_key(other).starts_with(&short));
            if !collides {
                break;
            }
            len += 1;
        }
        out.insert(key.to_string(), short_prompt_ref_with_len(key, len));
    }
    out
}

/// Normalize what a user pasted: whitespace, the brackets `--show-prompt`
/// prints around a reference, and an attestation key's `::t_` suffix.
pub fn normalize_prompt_query(query: &str) -> &str {
    prompt_key(
        query
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim(),
    )
}

/// How a prompt ID in a note matched a query, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PromptMatchKind {
    /// The query is the prompt or session ID.
    Exact,
    /// The query is the agent's own session ID (`agent_id.id`).
    AgentSession,
    /// The query is a prefix of the prompt or session ID.
    Prefix,
}

/// Prompts in one authorship log that match `query` (already normalized),
/// keyed by prompt ID. Only the best kind of match is returned: an exact ID
/// hides prefix matches.
pub fn matching_prompts_in_log(
    log: &AuthorshipLog,
    query: &str,
) -> (Option<PromptMatchKind>, BTreeMap<String, PromptRecord>) {
    let mut best: Option<PromptMatchKind> = None;
    let mut found: BTreeMap<String, PromptRecord> = BTreeMap::new();
    let prefix_allowed = hex_part(query).chars().count() >= MIN_PROMPT_REF_HEX;

    let mut consider = |id: &str, record: PromptRecord| {
        let kind = if id == query {
            PromptMatchKind::Exact
        } else if !record.agent_id.id.is_empty() && record.agent_id.id == query {
            PromptMatchKind::AgentSession
        } else if prefix_allowed && id.starts_with(query) {
            PromptMatchKind::Prefix
        } else {
            return;
        };
        match best {
            Some(b) if kind > b => {}
            Some(b) if kind == b => {
                found.entry(id.to_string()).or_insert(record);
            }
            _ => {
                best = Some(kind);
                found.clear();
                found.insert(id.to_string(), record);
            }
        }
    };

    for (id, record) in &log.metadata.prompts {
        consider(id, record.clone());
    }
    for (id, session) in &log.metadata.sessions {
        consider(id, session.to_prompt_record());
    }
    (best, found)
}

/// A prompt record found from a user-supplied ID.
#[derive(Debug, Clone)]
pub struct ResolvedPrompt {
    /// Commit whose authorship note holds the record.
    pub commit_sha: String,
    /// Full prompt or session ID (`s_<14 hex>` or a 16-hex legacy hash).
    pub prompt_id: String,
    pub record: PromptRecord,
}

/// Find a prompt in the repository history
///
/// If `commit` is provided, look only in that specific commit.
/// Otherwise, search through history and skip `offset` occurrences (0 = most recent).
pub fn find_prompt(
    repo: &Repository,
    prompt_id: &str,
    commit: Option<&str>,
    offset: usize,
) -> Result<(String, PromptRecord), AutterError> {
    resolve_prompt(repo, prompt_id, commit, offset).map(|r| (r.commit_sha, r.record))
}

/// Like [`find_prompt`], but also returns the full prompt ID the query
/// resolved to (a short reference or an agent session ID resolves to the
/// prompt or session key in the note).
pub fn resolve_prompt(
    repo: &Repository,
    query: &str,
    commit: Option<&str>,
    offset: usize,
) -> Result<ResolvedPrompt, AutterError> {
    if let Some(commit_rev) = commit {
        resolve_prompt_in_commit(repo, query, commit_rev)
    } else {
        resolve_prompt_in_history(repo, query, offset)
    }
}

/// Find a prompt in a specific commit (searches both prompts and sessions)
pub fn find_prompt_in_commit(
    repo: &Repository,
    prompt_id: &str,
    commit_rev: &str,
) -> Result<(String, PromptRecord), AutterError> {
    resolve_prompt_in_commit(repo, prompt_id, commit_rev).map(|r| (r.commit_sha, r.record))
}

fn resolve_prompt_in_commit(
    repo: &Repository,
    query: &str,
    commit_rev: &str,
) -> Result<ResolvedPrompt, AutterError> {
    // Resolve the revision to a commit SHA
    let commit = repo.revparse_single(commit_rev)?;
    let commit_sha = commit.id();

    // Get the authorship log for this commit
    let authorship_log = get_authorship(repo, &commit_sha).ok_or_else(|| {
        AutterError::Generic(format!(
            "No authorship data found for commit: {}",
            commit_rev
        ))
    })?;

    let normalized = normalize_prompt_query(query);
    let (_, matches) = matching_prompts_in_log(&authorship_log, normalized);
    match matches.len() {
        0 => Err(AutterError::Generic(format!(
            "Prompt '{}' not found in commit {}\n\
             Leave out --commit to search every commit's authorship note.",
            query, commit_rev
        ))),
        1 => {
            let (prompt_id, record) = matches.into_iter().next().expect("one match");
            Ok(ResolvedPrompt {
                commit_sha,
                prompt_id,
                record,
            })
        }
        _ => Err(ambiguous_error(
            query,
            matches
                .iter()
                .map(|(id, r)| (id.as_str(), r, commit_sha.as_str())),
        )),
    }
}

/// Find a prompt in history, skipping `offset` occurrences
/// Returns the (N+1)th occurrence where N = offset (0 = most recent)
pub fn find_prompt_in_history(
    repo: &Repository,
    prompt_id: &str,
    offset: usize,
) -> Result<(String, PromptRecord), AutterError> {
    resolve_prompt_in_history(repo, prompt_id, offset).map(|r| (r.commit_sha, r.record))
}

fn resolve_prompt_in_history(
    repo: &Repository,
    query: &str,
    offset: usize,
) -> Result<ResolvedPrompt, AutterError> {
    let normalized = normalize_prompt_query(query);
    if normalized.is_empty() {
        return Err(AutterError::Generic(
            "show-prompt requires a prompt ID".to_string(),
        ));
    }

    // Notes whose JSON holds a string starting with the query: a prompt or
    // session key, or an agent session ID. Newest commit first.
    let pattern = format!("\"{}", escape_grep_pattern(normalized));
    let mut shas = grep_ai_notes(repo, &pattern).unwrap_or_default();
    let notes_outside_git = crate::config::Config::get()
        .notes_backend_kind()
        .uses_http();
    if shas.is_empty() && notes_outside_git {
        shas = recent_commits(repo, NOTES_STORE_SCAN_LIMIT);
    }

    // Read each candidate note once and keep its matches.
    let mut per_commit: Vec<(String, PromptMatchKind, BTreeMap<String, PromptRecord>)> = Vec::new();
    for sha in &shas {
        if let Some(log) = get_authorship(repo, sha) {
            let (kind, matches) = matching_prompts_in_log(&log, normalized);
            if let Some(kind) = kind {
                per_commit.push((sha.clone(), kind, matches));
            }
        }
    }

    let Some(best) = per_commit.iter().map(|(_, k, _)| *k).min() else {
        return Err(AutterError::Generic(not_found_message(
            query,
            notes_outside_git,
        )));
    };

    // Every distinct prompt ID the best kind of match points at.
    let mut candidates: BTreeMap<&str, (&PromptRecord, &str)> = BTreeMap::new();
    for (sha, kind, matches) in &per_commit {
        if *kind == best {
            for (id, record) in matches {
                candidates
                    .entry(id.as_str())
                    .or_insert((record, sha.as_str()));
            }
        }
    }
    if candidates.len() > 1 {
        return Err(ambiguous_error(
            query,
            candidates
                .iter()
                .map(|(id, (record, sha))| (*id, *record, *sha)),
        ));
    }
    let prompt_id = candidates
        .keys()
        .next()
        .expect("at least one candidate")
        .to_string();

    // Occurrences of that ID, newest first.
    let occurrences: Vec<(&String, &PromptRecord)> = per_commit
        .iter()
        .filter_map(|(sha, _, matches)| matches.get(&prompt_id).map(|record| (sha, record)))
        .collect();
    match occurrences.get(offset) {
        Some((sha, record)) => Ok(ResolvedPrompt {
            commit_sha: (*sha).clone(),
            prompt_id,
            record: (*record).clone(),
        }),
        None => Err(AutterError::Generic(format!(
            "Prompt '{}' found {} time(s), but offset {} requested (max offset: {})",
            query,
            occurrences.len(),
            offset,
            occurrences.len().saturating_sub(1)
        ))),
    }
}

/// Why a prompt ID matched nothing, with where to find a real one.
fn not_found_message(query: &str, notes_outside_git: bool) -> String {
    let mut msg = format!(
        "Prompt not found in history: {query}\n\
         No authorship note on this machine has a prompt or session with that ID.\n\
         \n\
         Where to find a prompt ID:\n\
         \x20 autter blame <file>               legend of the prompts behind AI lines\n\
         \x20 autter blame --why <file>:<line>  the prompt behind one line\n\
         \x20 autter show <commit>              the raw note, prompts and sessions included\n\
         \n\
         Notes made on other machines arrive with `autter fetch-notes`."
    );
    if notes_outside_git {
        msg.push_str(
            "\nThis machine uses the http notes backend: only notes cached here are searched.",
        );
    }
    msg
}

fn ambiguous_error<'a>(
    query: &str,
    candidates: impl Iterator<Item = (&'a str, &'a PromptRecord, &'a str)>,
) -> AutterError {
    let rows: Vec<(&str, &str, &str, &str)> = candidates
        .map(|(id, record, sha)| {
            let model = if record.agent_id.model.is_empty() {
                "model not recorded"
            } else {
                record.agent_id.model.as_str()
            };
            (
                id,
                record.agent_id.tool.as_str(),
                model,
                &sha[..sha.len().min(8)],
            )
        })
        .collect();
    let id_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let tool_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
    let model_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(0);

    let mut msg = format!("Prompt ID '{query}' is ambiguous. It matches:\n");
    for (id, tool, model, sha) in rows {
        msg.push_str(&format!(
            "  {id:<id_w$}  {tool:<tool_w$}  {model:<model_w$}  commit {sha}\n"
        ));
    }
    msg.push_str("Use more characters, or the full ID.");
    AutterError::Generic(msg)
}

/// Escape `git grep`'s basic-regex metacharacters.
fn escape_grep_pattern(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '.' | '*' | '[' | ']' | '^' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn recent_commits(repo: &Repository, limit: usize) -> Vec<String> {
    let mut args = repo.global_args_for_exec();
    args.push("rev-list".to_string());
    args.push(format!("--max-count={limit}"));
    args.push("HEAD".to_string());
    match crate::git::repository::exec_git(&args) {
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::authorship_log::SessionRecord;
    use crate::authorship::working_log::AgentId;

    fn record(tool: &str, id: &str, model: &str) -> PromptRecord {
        PromptRecord {
            agent_id: AgentId {
                tool: tool.to_string(),
                id: id.to_string(),
                model: model.to_string(),
            },
            human_author: None,
            messages_url: None,
            total_additions: 0,
            total_deletions: 0,
            accepted_lines: 0,
            overriden_lines: 0,
            custom_attributes: None,
        }
    }

    fn session(tool: &str, id: &str, model: &str) -> SessionRecord {
        SessionRecord {
            agent_id: AgentId {
                tool: tool.to_string(),
                id: id.to_string(),
                model: model.to_string(),
            },
            human_author: None,
            messages_url: None,
            stats: None,
            custom_attributes: None,
        }
    }

    fn log_with(
        prompts: &[(&str, PromptRecord)],
        sessions: &[(&str, SessionRecord)],
    ) -> AuthorshipLog {
        let mut log = AuthorshipLog::new();
        for (id, r) in prompts {
            log.metadata.prompts.insert(id.to_string(), r.clone());
        }
        for (id, s) in sessions {
            log.metadata.sessions.insert(id.to_string(), s.clone());
        }
        log
    }

    #[test]
    fn short_refs_keep_session_prefix() {
        assert_eq!(
            short_prompt_ref("s_3f9a2c1d4e5f60::t_0123456789abcd"),
            "s_3f9a2c1d"
        );
        assert_eq!(short_prompt_ref("s_3f9a2c1d4e5f60"), "s_3f9a2c1d");
        assert_eq!(short_prompt_ref("0123456789abcdef"), "01234567");
        assert_eq!(short_prompt_ref("s_abc"), "s_abc");
    }

    #[test]
    fn short_refs_lengthen_on_collision() {
        let refs = short_prompt_refs([
            "s_3f9a2c1d4e5f60::t_aaaaaaaaaaaaaa",
            "s_3f9a2c1d9999aa",
            "s_0000000000000a",
        ]);
        assert_eq!(refs["s_3f9a2c1d4e5f60"], "s_3f9a2c1d4");
        assert_eq!(refs["s_3f9a2c1d9999aa"], "s_3f9a2c1d9");
        assert_eq!(refs["s_0000000000000a"], "s_00000000");
    }

    #[test]
    fn normalize_strips_brackets_and_trace() {
        assert_eq!(normalize_prompt_query(" [s_3f9a2c1d] "), "s_3f9a2c1d");
        assert_eq!(
            normalize_prompt_query("s_3f9a2c1d4e5f60::t_0123456789abcd"),
            "s_3f9a2c1d4e5f60"
        );
    }

    #[test]
    fn matches_exact_prefix_and_agent_session() {
        let log = log_with(
            &[(
                "s_3f9a2c1d4e5f60",
                record("claude", "uuid-1", "claude-opus-4"),
            )],
            &[(
                "s_3f9a2c1d4e5f60",
                session("claude", "uuid-1", "claude-opus-4"),
            )],
        );

        let (kind, m) = matching_prompts_in_log(&log, "s_3f9a2c1d4e5f60");
        assert_eq!(kind, Some(PromptMatchKind::Exact));
        assert_eq!(m.len(), 1);

        let (kind, m) = matching_prompts_in_log(&log, "s_3f9a2c1d");
        assert_eq!(kind, Some(PromptMatchKind::Prefix));
        assert!(m.contains_key("s_3f9a2c1d4e5f60"));

        let (kind, m) = matching_prompts_in_log(&log, "uuid-1");
        assert_eq!(kind, Some(PromptMatchKind::AgentSession));
        assert!(m.contains_key("s_3f9a2c1d4e5f60"));
    }

    #[test]
    fn too_short_prefix_does_not_match() {
        let log = log_with(&[("s_3f9a2c1d4e5f60", record("claude", "x", "m"))], &[]);
        let (kind, m) = matching_prompts_in_log(&log, "s_3f9");
        assert_eq!(kind, None);
        assert!(m.is_empty());
    }

    #[test]
    fn exact_match_hides_prefix_matches() {
        let log = log_with(
            &[
                ("0123456789abcdef", record("cursor", "a", "m")),
                ("0123456789abcdef00", record("cursor", "b", "m")),
            ],
            &[],
        );
        let (kind, m) = matching_prompts_in_log(&log, "0123456789abcdef");
        assert_eq!(kind, Some(PromptMatchKind::Exact));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn ambiguous_prefix_lists_every_candidate() {
        let log = log_with(
            &[
                ("s_3f9a2c1d4e5f60", record("claude", "a", "opus")),
                ("s_3f9a2c1d9999aa", record("codex", "b", "")),
            ],
            &[],
        );
        let (_, m) = matching_prompts_in_log(&log, "s_3f9a");
        assert_eq!(m.len(), 2);
        let err = ambiguous_error(
            "s_3f9a",
            m.iter().map(|(id, r)| (id.as_str(), r, "abcdef0123456789")),
        );
        insta::assert_snapshot!(err.to_string());
    }

    #[test]
    fn not_found_message_says_where_ids_come_from() {
        insta::assert_snapshot!(not_found_message("s_deadbeef", false));
    }

    #[test]
    fn grep_pattern_is_escaped() {
        assert_eq!(escape_grep_pattern("a.b*c[d]"), "a\\.b\\*c\\[d\\]");
    }
}
