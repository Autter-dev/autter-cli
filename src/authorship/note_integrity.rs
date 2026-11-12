//! Integrity check for authorship notes.
//!
//! A note lives in `refs/notes/ai`, which anyone with write access to the
//! repository can change. This module binds every note autter writes to the
//! commit it describes, and lets readers tell when a note does not belong to
//! its commit or was changed after autter wrote it.
//!
//! **What it binds.** When a note is written, autter appends one `integrity`
//! member to the note's JSON metadata:
//!
//! ```json
//! "integrity": {"v":1,"commit":"<sha>","tree":"<tree oid>","files":{"<path>":"<blob oid>"},"digest":"<sha256>"}
//! ```
//!
//! * `commit` and `tree`: the commit the note was written for and its tree.
//! * `files`: the blob OID of every attested file at that commit (`null`
//!   when the path is not in the commit's tree).
//! * `digest`: SHA-256 over the note without the `integrity` member and over
//!   the other `integrity` fields, so editing either is detected.
//!
//! The schema version stays `authorship/3.0.0`: released CLIs ignore the
//! unknown member and keep reading new notes, and notes written before this
//! existed read as [`IntegrityStatus::Legacy`].
//!
//! **What it does not do.** This is an integrity check, not tamper evidence:
//! the repository's owner can rewrite a note and recompute every field. Making
//! later edits visible to someone else needs a record outside the repository
//! (the server-side record planned as the next stage).
//!
//! **Rewrites.** Git copies notes during `rebase` and `commit --amend` when
//! `notes.rewriteRef` is set, before autter rewrites them. A copied note still
//! names the original commit; when that commit's patch equals the new one the
//! note is [`IntegrityStatus::PendingRewrite`], not tampering. Every note
//! autter writes, including its rewrites, is stamped again for the commit it
//! is written to (see [`stamp_note_entries`]).

use crate::authorship::authorship_log::LineRange;
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::error::AutterError;
use crate::git::repository::{Repository, exec_git_stdin};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Version of the `integrity` member's own format.
pub const INTEGRITY_FORMAT_VERSION: u32 = 1;

/// The JSON key of the integrity member inside a note's metadata.
const INTEGRITY_KEY: &str = "integrity";

/// The line prefix the integrity member is written with (two-space indent,
/// as `serde_json::to_string_pretty` writes the other members).
const INTEGRITY_LINE_PREFIX: &str = "  \"integrity\": ";

/// The integrity member of a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteIntegrity {
    pub v: u32,
    pub commit: String,
    pub tree: String,
    pub files: BTreeMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// Result of checking one commit's note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityStatus {
    /// The note was written for this commit and has not changed since.
    Verified,
    /// The note predates the integrity check (no `integrity` member). Its
    /// commit and line ranges were still checked.
    Legacy,
    /// The commit has no authorship note.
    Missing,
    /// Git copied another commit's note here during a rebase or amend, and
    /// autter has not rewritten it yet. The two commits carry the same patch.
    PendingRewrite,
    /// A note rewritten by an autter release older than the integrity check:
    /// it names this commit but its integrity member still names the commit
    /// it was first written for.
    ReboundByOlderAutter,
    /// The commit or the files the note describes are not in this clone
    /// (a shallow clone, for example), so the note could not be checked.
    Unavailable,
    /// The note names another commit, and that commit's change is not this
    /// one: it was copied or moved here.
    WrongCommit,
    /// The note's integrity member does not describe this commit's files.
    ContentMismatch,
    /// The note attributes lines past the end of a file, or lines in a file
    /// the commit does not contain.
    RangeOutOfBounds,
    /// The note was changed after autter wrote it.
    DigestMismatch,
}

impl IntegrityStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            IntegrityStatus::Verified => "verified",
            IntegrityStatus::Legacy => "legacy",
            IntegrityStatus::Missing => "missing",
            IntegrityStatus::PendingRewrite => "pending_rewrite",
            IntegrityStatus::ReboundByOlderAutter => "rebound_by_older_autter",
            IntegrityStatus::Unavailable => "unavailable",
            IntegrityStatus::WrongCommit => "wrong_commit",
            IntegrityStatus::ContentMismatch => "content_mismatch",
            IntegrityStatus::RangeOutOfBounds => "range_out_of_bounds",
            IntegrityStatus::DigestMismatch => "digest_mismatch",
        }
    }

    /// The note does not belong to this commit or was changed: its lines are
    /// not trusted and `autter verify` exits non-zero.
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            IntegrityStatus::WrongCommit
                | IntegrityStatus::ContentMismatch
                | IntegrityStatus::RangeOutOfBounds
                | IntegrityStatus::DigestMismatch
        )
    }

    /// Worth a warning, never a failure.
    pub fn is_warning(self) -> bool {
        matches!(
            self,
            IntegrityStatus::Legacy
                | IntegrityStatus::Missing
                | IntegrityStatus::PendingRewrite
                | IntegrityStatus::ReboundByOlderAutter
                | IntegrityStatus::Unavailable
        )
    }
}

/// One commit's result, with a sentence saying why.
#[derive(Debug, Clone, Serialize)]
pub struct CommitIntegrity {
    pub commit: String,
    pub status: IntegrityStatus,
    pub detail: String,
    /// The commit the note says it was written for, when that is another one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_commit: Option<String>,
}

// ---------------------------------------------------------------------------
// Note text: splitting off and inserting the integrity member
// ---------------------------------------------------------------------------

/// A note split into the text without its integrity member and the member.
struct SplitNote {
    /// The note without the integrity member, trimmed. This is what the
    /// digest covers and what is stamped again on rewrite.
    body: String,
    /// The integrity member as written, if any.
    integrity: Option<Result<NoteIntegrity, String>>,
    /// The metadata has an `integrity` key that is not on the line autter
    /// writes it on (the note was reformatted or edited by hand).
    integrity_moved: bool,
}

fn split_note(content: &str) -> SplitNote {
    let content = content.trim();
    let divider = content
        .find("\n---\n")
        .map(|i| i + 5)
        .or_else(|| content.strip_prefix("---\n").map(|_| 4));
    if let Some(json_start) = divider {
        let metadata = &content[json_start..];
        // The integrity member is written as its own line, as the last member.
        let mut offset = 0usize;
        for line in metadata.split_inclusive('\n') {
            if let Some(member) = line.strip_prefix(INTEGRITY_LINE_PREFIX) {
                let member_json = member.trim_end().trim_end_matches(',');
                let line_start = json_start + offset;
                let before = content[..line_start].trim_end();
                let before = before.strip_suffix(',').unwrap_or(before);
                let after = &content[line_start + line.len()..];
                let body = format!("{}\n{}", before, after.trim_start())
                    .trim()
                    .to_string();
                let integrity =
                    serde_json::from_str::<NoteIntegrity>(member_json).map_err(|e| e.to_string());
                return SplitNote {
                    body,
                    integrity: Some(integrity),
                    integrity_moved: false,
                };
            }
            offset += line.len();
        }
        let integrity_moved = serde_json::from_str::<serde_json::Value>(metadata)
            .ok()
            .and_then(|v| v.get(INTEGRITY_KEY).cloned())
            .is_some();
        return SplitNote {
            body: content.to_string(),
            integrity: None,
            integrity_moved,
        };
    }
    SplitNote {
        body: content.to_string(),
        integrity: None,
        integrity_moved: false,
    }
}

/// The note text with the integrity member removed, if it has one.
pub fn strip_integrity(content: &str) -> String {
    split_note(content).body
}

/// The integrity member of `content`, if it has a readable one.
pub fn read_integrity(content: &str) -> Option<NoteIntegrity> {
    split_note(content).integrity.and_then(Result::ok)
}

/// `content` with `integrity` written into it and a digest computed for it,
/// replacing any member it had. Anyone who can write to the repository can do
/// this, which is why this stage is an integrity check and not tamper
/// evidence (see the module docs).
pub fn seal_note(content: &str, integrity: &NoteIntegrity) -> Option<String> {
    insert_integrity(&split_note(content).body, integrity)
}

fn compute_digest(body: &str, integrity: &NoteIntegrity) -> String {
    let mut unsigned = integrity.clone();
    unsigned.digest = None;
    let fields = serde_json::to_string(&unsigned).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    hasher.update(b"\n--- integrity ---\n");
    hasher.update(fields.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Append `integrity` (with its digest) to `body`, a note without one.
fn insert_integrity(body: &str, integrity: &NoteIntegrity) -> Option<String> {
    let body = body.trim_end();
    let close = body.rfind('}')?;
    if !body[close + 1..].trim().is_empty() {
        return None;
    }
    let mut signed = integrity.clone();
    signed.digest = Some(compute_digest(body, integrity));
    let member = serde_json::to_string(&signed).ok()?;
    let before = body[..close].trim_end();
    Some(format!("{before},\n{INTEGRITY_LINE_PREFIX}{member}\n}}"))
}

// ---------------------------------------------------------------------------
// Batched object lookups
// ---------------------------------------------------------------------------

/// `git cat-file --batch-check` over `queries`; returns query -> (type, oid).
/// Queries that do not resolve are absent.
fn batch_check(
    repo: &Repository,
    queries: &[String],
) -> Result<HashMap<String, (String, String)>, AutterError> {
    let mut found = HashMap::new();
    if queries.is_empty() {
        return Ok(found);
    }
    let mut args = repo.global_args_for_exec();
    args.push("cat-file".to_string());
    args.push("--batch-check=%(objectname) %(objecttype)".to_string());
    let mut stdin = String::new();
    for q in queries {
        stdin.push_str(q);
        stdin.push('\n');
    }
    let output = exec_git_stdin(&args, stdin.as_bytes())?;
    let text = String::from_utf8_lossy(&output.stdout);
    // One output line per input line, in order.
    for (query, line) in queries.iter().zip(text.lines()) {
        let mut parts = line.split_whitespace();
        let (Some(oid), Some(kind)) = (parts.next(), parts.next()) else {
            continue;
        };
        if kind == "missing" || kind == "ambiguous" {
            continue;
        }
        found.insert(query.clone(), (kind.to_string(), oid.to_string()));
    }
    Ok(found)
}

/// Line counts of blobs, read with one `git cat-file --batch`.
fn blob_line_counts(
    repo: &Repository,
    blob_oids: &[String],
) -> Result<HashMap<String, u32>, AutterError> {
    let mut counts = HashMap::new();
    if blob_oids.is_empty() {
        return Ok(counts);
    }
    let contents = crate::git::refs::batch_read_blob_contents(repo, blob_oids)?;
    for (oid, text) in contents {
        let lines = if text.is_empty() {
            0
        } else {
            let n = text.matches('\n').count();
            if text.ends_with('\n') { n } else { n + 1 }
        };
        counts.insert(oid, lines as u32);
    }
    Ok(counts)
}

fn tree_query(commit: &str) -> String {
    format!("{commit}^{{tree}}")
}

fn path_query(commit: &str, path: &str) -> String {
    format!("{commit}:{path}")
}

// ---------------------------------------------------------------------------
// Stamping
// ---------------------------------------------------------------------------

/// Stamp each `(commit, note)` with an integrity member for that commit.
///
/// Notes that are not authorship logs, or whose commit is not in the
/// repository, are written as they are (without an integrity member, so they
/// read as legacy rather than as tampered). Never fails the write: on any
/// error the notes are returned unstamped.
pub fn stamp_note_entries(
    repo: &Repository,
    entries: &[(String, String)],
) -> Vec<(String, String)> {
    match try_stamp_note_entries(repo, entries) {
        Ok(stamped) => stamped,
        Err(e) => {
            tracing::debug!("note integrity: stamping skipped: {e}");
            entries
                .iter()
                .map(|(c, n)| (c.clone(), strip_integrity_if_note(n)))
                .collect()
        }
    }
}

/// Remove a stale integrity member (one written for another commit) when the
/// note cannot be stamped again, so it reads as legacy, not as tampering.
fn strip_integrity_if_note(note: &str) -> String {
    let split = split_note(note);
    if split.integrity.is_some() {
        split.body
    } else {
        note.to_string()
    }
}

fn try_stamp_note_entries(
    repo: &Repository,
    entries: &[(String, String)],
) -> Result<Vec<(String, String)>, AutterError> {
    struct Pending {
        body: String,
        files: Vec<String>,
    }
    let mut pending: Vec<Option<Pending>> = Vec::with_capacity(entries.len());
    let mut queries: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for (commit, note) in entries {
        let split = split_note(note);
        let Ok(log) = AuthorshipLog::deserialize_from_string(&split.body) else {
            pending.push(None);
            continue;
        };
        let files: Vec<String> = log
            .attestations
            .iter()
            .map(|a| a.file_path.clone())
            .collect();
        for q in
            std::iter::once(tree_query(commit)).chain(files.iter().map(|f| path_query(commit, f)))
        {
            if seen.insert(q.clone()) {
                queries.push(q);
            }
        }
        pending.push(Some(Pending {
            body: split.body,
            files,
        }));
    }
    let objects = batch_check(repo, &queries)?;

    let mut out = Vec::with_capacity(entries.len());
    for ((commit, note), pending) in entries.iter().zip(pending) {
        let Some(p) = pending else {
            out.push((commit.clone(), note.clone()));
            continue;
        };
        let Some((_, tree)) = objects.get(&tree_query(commit)) else {
            out.push((commit.clone(), p.body));
            continue;
        };
        let files = p
            .files
            .iter()
            .map(|f| {
                let oid = objects
                    .get(&path_query(commit, f))
                    .filter(|(kind, _)| kind == "blob")
                    .map(|(_, oid)| oid.clone());
                (f.clone(), oid)
            })
            .collect();
        let integrity = NoteIntegrity {
            v: INTEGRITY_FORMAT_VERSION,
            commit: commit.clone(),
            tree: tree.clone(),
            files,
            digest: None,
        };
        match insert_integrity(&p.body, &integrity) {
            Some(stamped) => out.push((commit.clone(), stamped)),
            None => out.push((commit.clone(), p.body)),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Check the notes of `commits` (full SHAs). `notes` maps commit -> raw note
/// text for the commits that have one. Uses a fixed number of git processes
/// however many commits there are.
pub fn verify_notes(
    repo: &Repository,
    commits: &[String],
    notes: &HashMap<String, String>,
) -> Result<Vec<CommitIntegrity>, AutterError> {
    struct Parsed {
        split: SplitNote,
        log: Option<AuthorshipLog>,
        recorded: String,
    }
    let mut parsed: HashMap<&str, Parsed> = HashMap::new();
    let mut queries: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |q: String, queries: &mut Vec<String>| {
        if seen.insert(q.clone()) {
            queries.push(q);
        }
    };
    for commit in commits {
        let Some(note) = notes.get(commit) else {
            continue;
        };
        let split = split_note(note);
        let log = AuthorshipLog::deserialize_from_string(&split.body).ok();
        let recorded = log
            .as_ref()
            .map(|l| l.metadata.base_commit_sha.clone())
            .unwrap_or_default();
        push(tree_query(commit), &mut queries);
        if let Some(log) = &log {
            for a in &log.attestations {
                push(path_query(commit, &a.file_path), &mut queries);
            }
        }
        parsed.insert(
            commit.as_str(),
            Parsed {
                split,
                log,
                recorded,
            },
        );
    }
    let objects = batch_check(repo, &queries)?;

    // Line counts of every attested blob, for the range check.
    let mut blob_oids: Vec<String> = Vec::new();
    let mut seen_blobs = HashSet::new();
    for (commit, p) in &parsed {
        if let Some(log) = &p.log {
            for a in &log.attestations {
                if let Some((kind, oid)) = objects.get(&path_query(commit, &a.file_path))
                    && kind == "blob"
                    && seen_blobs.insert(oid.clone())
                {
                    blob_oids.push(oid.clone());
                }
            }
        }
    }
    let line_counts = blob_line_counts(repo, &blob_oids)?;

    // Which notes name another commit; compare patches for those in one go.
    let mut other_commits: Vec<(String, String)> = Vec::new();
    for (commit, p) in &parsed {
        let named = match &p.split.integrity {
            Some(Ok(i)) if i.commit != *commit && p.recorded != *commit => Some(i.commit.clone()),
            _ if !p.recorded.is_empty() && p.recorded != *commit => Some(p.recorded.clone()),
            _ => None,
        };
        if let Some(named) = named {
            other_commits.push((commit.to_string(), named));
        }
    }
    let patch_ids = patch_ids_for(
        repo,
        &other_commits
            .iter()
            .flat_map(|(a, b)| [a.clone(), b.clone()])
            .collect::<Vec<_>>(),
    );

    let mut results = Vec::with_capacity(commits.len());
    for commit in commits {
        let Some(p) = parsed.get(commit.as_str()) else {
            results.push(CommitIntegrity {
                commit: commit.clone(),
                status: IntegrityStatus::Missing,
                detail: "no authorship note".to_string(),
                recorded_commit: None,
            });
            continue;
        };
        results.push(check_one(
            commit,
            p.log.as_ref(),
            &p.split,
            &p.recorded,
            &objects,
            &line_counts,
            &patch_ids,
        ));
    }
    Ok(results)
}

#[allow(clippy::too_many_arguments)]
fn check_one(
    commit: &str,
    log: Option<&AuthorshipLog>,
    split: &SplitNote,
    recorded: &str,
    objects: &HashMap<String, (String, String)>,
    line_counts: &HashMap<String, u32>,
    patch_ids: &HashMap<String, String>,
) -> CommitIntegrity {
    let result = |status: IntegrityStatus, detail: String, recorded_commit: Option<String>| {
        CommitIntegrity {
            commit: commit.to_string(),
            status,
            detail,
            recorded_commit,
        }
    };
    let Some(log) = log else {
        return result(
            IntegrityStatus::DigestMismatch,
            "the note is not a readable authorship record".to_string(),
            None,
        );
    };
    let Some((_, actual_tree)) = objects.get(&tree_query(commit)) else {
        return result(
            IntegrityStatus::Unavailable,
            "the commit is not in this clone (shallow clone?), so its note could not be checked"
                .to_string(),
            None,
        );
    };
    let same_patch = |other: &str| -> bool {
        match (patch_ids.get(commit), patch_ids.get(other)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    };
    let moved = |named: &str| -> CommitIntegrity {
        if same_patch(named) {
            result(
                IntegrityStatus::PendingRewrite,
                format!(
                    "git copied the note of {} (same change) here during a rewrite; autter has not rewritten it yet",
                    short(named)
                ),
                Some(named.to_string()),
            )
        } else {
            result(
                IntegrityStatus::WrongCommit,
                format!(
                    "the note was written for {}, a different change, and copied or moved to this commit",
                    short(named)
                ),
                Some(named.to_string()),
            )
        }
    };

    if split.integrity_moved {
        return result(
            IntegrityStatus::DigestMismatch,
            "the note's integrity record was moved or reformatted after autter wrote it"
                .to_string(),
            None,
        );
    }

    match &split.integrity {
        Some(Err(e)) => result(
            IntegrityStatus::DigestMismatch,
            format!("the note's integrity record is not readable ({e})"),
            None,
        ),
        Some(Ok(integrity)) => {
            if integrity.commit != commit {
                if recorded == commit {
                    let rangecheck = range_check(commit, log, objects, line_counts);
                    if let Some(r) = rangecheck {
                        return r;
                    }
                    return result(
                        IntegrityStatus::ReboundByOlderAutter,
                        format!(
                            "an autter release older than the integrity check moved this note from {}; run a newer autter over the range to stamp it again",
                            short(&integrity.commit)
                        ),
                        Some(integrity.commit.clone()),
                    );
                }
                return moved(&integrity.commit);
            }
            let expected = compute_digest(&split.body, integrity);
            if integrity.digest.as_deref() != Some(expected.as_str()) {
                return result(
                    IntegrityStatus::DigestMismatch,
                    "the note was changed after autter wrote it".to_string(),
                    None,
                );
            }
            // The stamp names this commit and its digest holds, so autter wrote
            // the note for this commit: a `base_commit_sha` naming another one
            // is stale metadata from the writer, not a moved note.
            if integrity.tree != *actual_tree {
                return result(
                    IntegrityStatus::ContentMismatch,
                    "the note's record of the commit's tree does not match the commit".to_string(),
                    None,
                );
            }
            for a in &log.attestations {
                let actual = objects
                    .get(&path_query(commit, &a.file_path))
                    .filter(|(kind, _)| kind == "blob")
                    .map(|(_, oid)| oid.clone());
                match integrity.files.get(&a.file_path) {
                    Some(recorded_blob) if *recorded_blob == actual => {}
                    Some(_) => {
                        return result(
                            IntegrityStatus::ContentMismatch,
                            format!(
                                "the note describes a different version of {} than this commit has",
                                a.file_path
                            ),
                            None,
                        );
                    }
                    None => {
                        return result(
                            IntegrityStatus::ContentMismatch,
                            format!(
                                "the note attributes lines in {}, which its integrity record does not cover",
                                a.file_path
                            ),
                            None,
                        );
                    }
                }
            }
            if let Some(r) = range_check(commit, log, objects, line_counts) {
                return r;
            }
            result(
                IntegrityStatus::Verified,
                "written for this commit and unchanged".to_string(),
                None,
            )
        }
        None => {
            if !recorded.is_empty() && recorded != commit {
                return moved(recorded);
            }
            if let Some(r) = range_check(commit, log, objects, line_counts) {
                return r;
            }
            result(
                IntegrityStatus::Legacy,
                "written before the integrity check: the commit and line ranges match, but changes to the note cannot be detected"
                    .to_string(),
                None,
            )
        }
    }
}

/// Lines attributed past the end of a file, or in a file the commit lacks.
fn range_check(
    commit: &str,
    log: &AuthorshipLog,
    objects: &HashMap<String, (String, String)>,
    line_counts: &HashMap<String, u32>,
) -> Option<CommitIntegrity> {
    let fail = |detail: String, status: IntegrityStatus| {
        Some(CommitIntegrity {
            commit: commit.to_string(),
            status,
            detail,
            recorded_commit: None,
        })
    };
    for a in &log.attestations {
        let max_line = a
            .entries
            .iter()
            .flat_map(|e| e.line_ranges.iter())
            .map(|r| match r {
                LineRange::Single(l) => *l,
                LineRange::Range(_, end) => *end,
            })
            .max();
        let Some(max_line) = max_line else {
            continue;
        };
        let blob = objects
            .get(&path_query(commit, &a.file_path))
            .filter(|(kind, _)| kind == "blob")
            .map(|(_, oid)| oid);
        let Some(blob) = blob else {
            return fail(
                format!(
                    "the note attributes lines in {}, which this commit does not contain",
                    a.file_path
                ),
                IntegrityStatus::RangeOutOfBounds,
            );
        };
        let Some(lines) = line_counts.get(blob) else {
            return fail(
                format!(
                    "{} is not in this clone (shallow clone?), so its line ranges could not be checked",
                    a.file_path
                ),
                IntegrityStatus::Unavailable,
            );
        };
        if max_line > *lines {
            return fail(
                format!(
                    "the note attributes line {} of {}, which has {} lines in this commit",
                    max_line, a.file_path, lines
                ),
                IntegrityStatus::RangeOutOfBounds,
            );
        }
    }
    None
}

/// `git patch-id --stable` for each commit, from one `git log -p` and one
/// `git patch-id`. Commits without a patch (merges, empty commits, objects not
/// in this clone) are absent.
fn patch_ids_for(repo: &Repository, commits: &[String]) -> HashMap<String, String> {
    let mut ids = HashMap::new();
    let mut unique: Vec<String> = commits.to_vec();
    unique.sort();
    unique.dedup();
    if unique.is_empty() {
        return ids;
    }
    // Only commits that exist: a missing one would fail the whole `git log`.
    let present = match batch_check(repo, &unique) {
        Ok(found) => found,
        Err(_) => return ids,
    };
    let present: Vec<String> = unique
        .into_iter()
        .filter(|c| present.get(c).is_some_and(|(kind, _)| kind == "commit"))
        .collect();
    if present.is_empty() {
        return ids;
    }
    let mut args = repo.global_args_for_exec();
    args.extend(
        [
            "log",
            "--no-walk=unsorted",
            "-p",
            "--no-color",
            "--no-ext-diff",
            "--format=commit %H",
            "--stdin",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let stdin = present.join("\n") + "\n";
    let Ok(patches) = exec_git_stdin(&args, stdin.as_bytes()) else {
        return ids;
    };
    let mut args = repo.global_args_for_exec();
    args.push("patch-id".to_string());
    args.push("--stable".to_string());
    let Ok(out) = exec_git_stdin(&args, &patches.stdout) else {
        return ids;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        if let (Some(patch), Some(commit)) = (parts.next(), parts.next()) {
            ids.insert(commit.to_string(), patch.to_string());
        }
    }
    ids
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// Check the notes of `commits` as stored (git notes or the configured
/// notes backend).
pub fn verify_commits(
    repo: &Repository,
    commits: &[String],
) -> Result<Vec<CommitIntegrity>, AutterError> {
    let notes = crate::git::notes_api::read_notes_batch(repo, commits)?;
    verify_notes(repo, commits, &notes)
}

/// The one line readers print when a commit's note fails the check.
pub fn failure_notice(result: &CommitIntegrity) -> String {
    format!(
        "Autter's record for {} {}: its lines count as unknown. Run `autter verify {}` for details.",
        short(&result.commit),
        match result.status {
            IntegrityStatus::WrongCommit => "belongs to another commit",
            IntegrityStatus::RangeOutOfBounds => "does not match this commit's files",
            IntegrityStatus::ContentMismatch => "does not match this commit's files",
            _ => "was changed after autter wrote it",
        },
        short(&result.commit)
    )
}

// ---------------------------------------------------------------------------
// Readers: stats and blame
// ---------------------------------------------------------------------------

/// The commits among `commits` whose note fails the check, with why. Commits
/// without a note, legacy notes and warnings are not included. Never fails:
/// if the check cannot run, readers fall back to trusting the notes.
pub fn failed_notes(repo: &Repository, commits: &[String]) -> HashMap<String, CommitIntegrity> {
    let mut unique: Vec<String> = commits.to_vec();
    unique.sort();
    unique.dedup();
    match verify_commits(repo, &unique) {
        Ok(results) => results
            .into_iter()
            .filter(|r| r.status.is_failure())
            .map(|r| (r.commit.clone(), r))
            .collect(),
        Err(e) => {
            tracing::debug!("note integrity: check skipped: {e}");
            HashMap::new()
        }
    }
}

/// [`failed_notes`] for one commit.
pub fn failed_note(repo: &Repository, commit_sha: &str) -> Option<CommitIntegrity> {
    let full = repo
        .revparse_single(commit_sha)
        .ok()
        .map(|o| o.id().to_string())
        .unwrap_or_else(|| commit_sha.to_string());
    failed_notes(repo, &[full]).into_values().next()
}

/// Print [`failure_notice`] on stderr, once per commit per process.
pub fn report_failure(result: &CommitIntegrity) {
    use std::sync::{Mutex, OnceLock};
    static REPORTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let reported = REPORTED.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut set) = reported.lock()
        && set.insert(result.commit.clone())
    {
        eprintln!("{}", failure_notice(result));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "src/main.rs\n  abcd1234abcd1234 1-3\n---\n{\n  \"schema_version\": \"authorship/3.0.0\",\n  \"autter_version\": \"development\",\n  \"base_commit_sha\": \"c0ffee\",\n  \"prompts\": {}\n}";

    fn integrity() -> NoteIntegrity {
        NoteIntegrity {
            v: 1,
            commit: "c0ffee".to_string(),
            tree: "7ree".to_string(),
            files: BTreeMap::from([("src/main.rs".to_string(), Some("b10b".to_string()))]),
            digest: None,
        }
    }

    #[test]
    fn stamped_note_splits_back_to_the_original_text() {
        let stamped = insert_integrity(NOTE, &integrity()).unwrap();
        assert!(stamped.contains("\n  \"integrity\": {\"v\":1,"));
        let split = split_note(&stamped);
        assert_eq!(split.body, NOTE);
        let parsed = split.integrity.unwrap().unwrap();
        assert_eq!(parsed.commit, "c0ffee");
        assert_eq!(parsed.digest, Some(compute_digest(NOTE, &integrity())));
        // Released CLIs ignore the member and still read the note.
        let log = AuthorshipLog::deserialize_from_string(&stamped).unwrap();
        assert_eq!(log.metadata.base_commit_sha, "c0ffee");
        assert_eq!(log.attestations.len(), 1);
    }

    #[test]
    fn stamping_twice_replaces_the_member() {
        let once = insert_integrity(NOTE, &integrity()).unwrap();
        let mut other = integrity();
        other.commit = "beef".to_string();
        let twice = insert_integrity(&split_note(&once).body, &other).unwrap();
        assert_eq!(twice.matches("\"integrity\"").count(), 1);
        assert_eq!(
            split_note(&twice).integrity.unwrap().unwrap().commit,
            "beef"
        );
    }

    #[test]
    fn digest_covers_the_body_and_the_integrity_fields() {
        let base = compute_digest(NOTE, &integrity());
        assert_ne!(
            base,
            compute_digest(&NOTE.replace("1-3", "1-4"), &integrity())
        );
        let mut other = integrity();
        other.tree = "other".to_string();
        assert_ne!(base, compute_digest(NOTE, &other));
        // The digest field itself is not part of what it covers.
        let mut with_digest = integrity();
        with_digest.digest = Some("x".to_string());
        assert_eq!(base, compute_digest(NOTE, &with_digest));
    }

    #[test]
    fn a_reformatted_integrity_member_is_noticed() {
        let stamped = insert_integrity(NOTE, &integrity()).unwrap();
        let reformatted = stamped.replace("  \"integrity\": ", "    \"integrity\":  ");
        let split = split_note(&reformatted);
        assert!(split.integrity.is_none());
        assert!(split.integrity_moved);
    }

    #[test]
    fn notes_without_a_member_are_untouched_by_split() {
        let split = split_note(NOTE);
        assert_eq!(split.body, NOTE);
        assert!(split.integrity.is_none());
        assert!(!split.integrity_moved);
        assert_eq!(strip_integrity("not a note"), "not a note");
    }

    #[test]
    fn failures_and_warnings_do_not_overlap() {
        use IntegrityStatus::*;
        for s in [
            Verified,
            Legacy,
            Missing,
            PendingRewrite,
            ReboundByOlderAutter,
            Unavailable,
            WrongCommit,
            ContentMismatch,
            RangeOutOfBounds,
            DigestMismatch,
        ] {
            assert!(!(s.is_failure() && s.is_warning()), "{}", s.as_str());
            assert_eq!(s == Verified, !s.is_failure() && !s.is_warning());
        }
    }
}
