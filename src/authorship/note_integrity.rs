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
//! **A removed stamp.** Anyone can delete the `integrity` member, edit the
//! note, and have it read as legacy. So an unstamped note passes as legacy
//! only when it can be one:
//!
//! * its metadata's `autter_version` names autter 3.8.0 or later: every such
//!   release stamps every note it writes, so the stamp was removed
//!   ([`IntegrityStatus::StampRemoved`], a failure);
//! * its commit comes after stamping began, meaning it descends from a commit
//!   whose note is stamped (commit topology, not dates; see
//!   `note_stamp_floor`), and it has no readable `autter_version`: also
//!   [`IntegrityStatus::StampRemoved`];
//! * its commit comes after stamping began and its `autter_version` names a
//!   release older than 3.8.0: [`IntegrityStatus::UnstampedByOlderAutter`],
//!   a warning (a failure under `autter verify --strict`), because a teammate
//!   who has not upgraded writes exactly this, and the version cannot be
//!   checked: whoever removes a stamp can edit it too. This mirrors
//!   [`IntegrityStatus::ReboundByOlderAutter`];
//! * otherwise (older release, or no version, on a commit before stamping
//!   began or with no stamped notes in the repository): legacy.
//!
//! The paths that legitimately write unstamped notes after stamping began
//! all stay out of the failure: `autter notes backfill` copies notes as
//! stored, so a pre-3.8.0 note keeps its old version and an old commit; a
//! note rebound by an older autter keeps a stamp and is not unstamped; a
//! teammate on an older autter writes its own older version; and every
//! rewrite this autter performs (rebase, amend, cherry-pick, `autter ci`)
//! stamps the note it writes.
//!
//! **What it does not do.** On its own this is an integrity check, not tamper
//! evidence: anyone who can write the repository can rewrite a note and
//! recompute every field. In connected mode the server keeps every version of
//! each note it receives, and `autter verify` compares notes with that record
//! (`note_server_record`, statuses [`IntegrityStatus::ChangedAfterRecorded`]
//! and [`IntegrityStatus::NotRecorded`]), so a later edit is visible to the
//! organisation. A note that was never uploaded (local-only mode, or not
//! uploaded yet) can only get the integrity check.
//!
//! **Rewrites.** Git copies notes during `rebase` and `commit --amend` when
//! `notes.rewriteRef` is set, before autter rewrites them. A copied note still
//! names the original commit; when that commit's patch equals the new one the
//! note is [`IntegrityStatus::PendingRewrite`], not tampering. Every note
//! autter writes, including its rewrites, is stamped again for the commit it
//! is written to (see [`stamp_note_entries`]).
//!
//! **Carrying a failing note.** Stamping a rewritten note again must not turn
//! a note that fails into one that verifies. Before a rewrite carries
//! attribution from source notes to new commits (rebase, amend, cherry-pick,
//! `autter ci` merges) it checks the source notes in one pass
//! ([`check_rewrite_sources`]). A new commit derived from a failing note gets
//! a `carried_failure` field in its stamp, naming the source commit, and reads
//! as [`IntegrityStatus::CarriedFailure`]. Rewrites that rebuild a working log
//! instead (reset, `cherry-pick -n`, `merge --squash`) read attribution
//! through blame, which already skips a failing note, so its lines are not
//! carried at all.
//!
//! **Notes rebound by an older autter.** An autter older than the integrity
//! check that rewrites a note keeps the old stamp, so the digest cannot hold
//! and the note reads as [`IntegrityStatus::ReboundByOlderAutter`], a warning
//! (a failure under `autter verify --strict`). Anyone can make a note look
//! rebound, so what can still be checked is: the stamp's tree and blobs must
//! describe the commit it names when that commit is in this clone
//! ([`IntegrityStatus::ContentMismatch`] otherwise), and when that commit's
//! own note fails, the rebound note is [`IntegrityStatus::CarriedFailure`].
//!
//! **Stash notes.** `git stash` saves the working log as a note in
//! `refs/notes/ai-stash`, stamped for the stash commit
//! ([`seal_stash_note`]); `stash pop`/`apply` checks it
//! ([`check_stash_note`]) and does not restore one that fails. An unstamped
//! stash note is not restored when it says autter 3.8.2 or later wrote it
//! (the first release that stamps stash notes), or has no version and the
//! stash was made on a commit at or after the first stamped commit. One that
//! says 3.8.1 or earlier is restored, since 3.8.0 and 3.8.1 stamped commit
//! notes but not stash notes.
//!
//! **Notes copied from the notes database** (`autter notes backfill`) are
//! written as stored, without a new stamp, so a failing or legacy note is not
//! turned into a verified one (`git::notes_backfill`).

use crate::authorship::authorship_log::LineRange;
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::error::AutterError;
use crate::git::repository::{Repository, exec_git_stdin};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

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
    /// Set when autter derived this note, during a rewrite, from a note that
    /// failed the check. Covered by the digest like the other fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carried_failure: Option<CarriedFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// The failing note a rewritten note was derived from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedFailure {
    /// The commit whose note autter carried attribution from.
    pub commit: String,
    /// That note's status when autter carried it (`digest_mismatch`, ...).
    pub status: String,
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
    /// it was first written for. Its digest cannot be checked; what its stamp
    /// says about that commit was checked when the commit is in this clone.
    /// A warning, but a failure under `autter verify --strict`.
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
    /// Autter derived this note during a rewrite (rebase, amend, cherry-pick,
    /// `autter ci` merge) from a note that failed the check, or an older
    /// autter rebound it from a note that fails the check. The result names
    /// the source commit; its lines are no more trusted than the source's.
    CarriedFailure,
    /// Set only by `autter verify` in connected mode: the server's record of
    /// this note has more than one version, or the note here matches none of
    /// them or only a later one. The note was changed after it was uploaded
    /// (see `note_server_record`).
    ChangedAfterRecorded,
    /// Set only by `autter verify` in connected mode: the note passed the
    /// local check but the server has no record of it yet (notes upload in
    /// the background). A warning; a failure with `--require-record`.
    NotRecorded,
    /// The note has no integrity stamp, but it cannot have been written
    /// before stamping existed: its metadata says autter 3.8.0 or later wrote
    /// it (every such release stamps every note), or it has no readable
    /// autter version and its commit descends from a commit whose note is
    /// stamped. The stamp was removed.
    StampRemoved,
    /// The note has no integrity stamp and its metadata says an autter older
    /// than 3.8.0 wrote it, but its commit descends from a commit whose note
    /// is stamped. A teammate on an older autter writes such notes; so does
    /// anyone who removes a stamp and edits the version. A warning, but a
    /// failure under `autter verify --strict`.
    UnstampedByOlderAutter,
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
            IntegrityStatus::CarriedFailure => "carried_failure",
            IntegrityStatus::ChangedAfterRecorded => "changed_after_recorded",
            IntegrityStatus::NotRecorded => "not_recorded",
            IntegrityStatus::StampRemoved => "stamp_removed",
            IntegrityStatus::UnstampedByOlderAutter => "unstamped_by_older_autter",
        }
    }

    /// The note does not belong to this commit, was changed, or was derived
    /// from a note that failed: its lines are not trusted and `autter verify`
    /// exits non-zero.
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            IntegrityStatus::WrongCommit
                | IntegrityStatus::ContentMismatch
                | IntegrityStatus::RangeOutOfBounds
                | IntegrityStatus::DigestMismatch
                | IntegrityStatus::CarriedFailure
                | IntegrityStatus::ChangedAfterRecorded
                | IntegrityStatus::StampRemoved
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
                | IntegrityStatus::NotRecorded
                | IntegrityStatus::UnstampedByOlderAutter
        )
    }
}

// ---------------------------------------------------------------------------
// Unstamped notes: which autter says it wrote them
// ---------------------------------------------------------------------------

/// The first release that stamps every authorship note it writes.
pub const FIRST_STAMPING_RELEASE: (u64, u64, u64) = (3, 8, 0);

/// The first release that stamps every stash note it writes.
pub const FIRST_STASH_STAMPING_RELEASE: (u64, u64, u64) = (3, 8, 2);

/// What an unstamped note's `autter_version` says about who wrote it,
/// compared with the first release that would have stamped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionClaim {
    /// A release that stamps (this version or later): the stamp was removed.
    Stamping(String),
    /// A release older than that: it may well be legacy.
    Older(String),
    /// No version, or one that is not a release number.
    Unknown,
}

/// `autter_version` as `(major, minor, patch)`. Release builds write
/// `3.9.1`, debug builds `development:3.9.1`; a pre-release or build suffix
/// is ignored. `None` for anything else (`development` alone, missing).
fn release_of(version: &str) -> Option<(u64, u64, u64)> {
    let version = version.trim();
    let version = version.strip_prefix("development:").unwrap_or(version);
    let version = version.strip_prefix('v').unwrap_or(version);
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// What `log`'s `autter_version` claims, against `first_stamping`.
pub fn version_claim(log: &AuthorshipLog, first_stamping: (u64, u64, u64)) -> VersionClaim {
    let Some(version) = log.metadata.autter_version.as_deref() else {
        return VersionClaim::Unknown;
    };
    match release_of(version) {
        Some(release) if release >= first_stamping => VersionClaim::Stamping(version.to_string()),
        Some(_) => VersionClaim::Older(version.to_string()),
        None => VersionClaim::Unknown,
    }
}

fn release_name((major, minor, patch): (u64, u64, u64)) -> String {
    format!("{major}.{minor}.{patch}")
}

/// One commit's result, with a sentence saying why.
#[derive(Debug, Clone, Serialize)]
pub struct CommitIntegrity {
    pub commit: String,
    pub status: IntegrityStatus,
    pub detail: String,
    /// The commit the note says it was written for, when that is another one;
    /// for [`IntegrityStatus::CarriedFailure`], the commit whose failing note
    /// it was derived from.
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

/// Whether `note` is a note autter wrote for `commit`: its integrity member
/// names `commit`, or, for a note without one (written before the integrity
/// check), its `base_commit_sha` does. A note git copied here from another
/// commit during a rebase or amend names that other commit, so it is not.
pub fn note_written_for(note: &str, commit: &str) -> bool {
    let split = split_note(note);
    match split.integrity {
        Some(Ok(integrity)) => integrity.commit == commit,
        Some(Err(_)) => false,
        None => AuthorshipLog::deserialize_from_string(&split.body)
            .map(|log| log.metadata.base_commit_sha == commit)
            .unwrap_or(false),
    }
}

/// `content` with `integrity` written into it and a digest computed for it,
/// replacing any member it had. Anyone who can write to the repository can do
/// this, which is why the digest alone is an integrity check and not tamper
/// evidence; the server's record covers that (see the module docs).
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
///
/// A note for a commit that [`check_rewrite_sources`] found derived from a
/// failing note is stamped with `carried_failure`, also when its objects
/// cannot be looked up, so it never reads as legacy or verified.
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
                .map(|(c, n)| (c.clone(), strip_integrity_if_note(c, n)))
                .collect()
        }
    }
}

/// Remove a stale integrity member (one written for another commit) when the
/// note cannot be stamped again, so it reads as legacy, not as tampering.
/// A note carried from a failing note keeps saying so instead.
fn strip_integrity_if_note(commit: &str, note: &str) -> String {
    let split = split_note(note);
    if let Some(stamped) = stamp_carried_without_objects(commit, &split.body) {
        return stamped;
    }
    if split.integrity.is_some() {
        split.body
    } else {
        note.to_string()
    }
}

/// `body` stamped with only its carried failure, for when the commit's tree
/// and files cannot be looked up. `None` when `commit` carries no failure or
/// `body` is not a note.
fn stamp_carried_without_objects(commit: &str, body: &str) -> Option<String> {
    let carried = carried_failure_for(commit)?;
    AuthorshipLog::deserialize_from_string(body).ok()?;
    insert_integrity(
        body,
        &NoteIntegrity {
            v: INTEGRITY_FORMAT_VERSION,
            commit: commit.to_string(),
            tree: String::new(),
            files: BTreeMap::new(),
            carried_failure: Some(carried),
            digest: None,
        },
    )
}

/// New commit -> the failing note it is being derived from, for the rewrites
/// in progress in this process. Filled by [`check_rewrite_sources`] and read
/// when the rewritten notes are stamped. Keyed by commit SHA, so rewrites in
/// several repositories at once (the background service) do not mix.
fn carried_failures() -> &'static Mutex<HashMap<String, CarriedFailure>> {
    static CARRIED: OnceLock<Mutex<HashMap<String, CarriedFailure>>> = OnceLock::new();
    CARRIED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn carried_failure_for(commit: &str) -> Option<CarriedFailure> {
    carried_failures()
        .lock()
        .ok()
        .and_then(|map| map.get(commit).cloned())
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
            let note = stamp_carried_without_objects(commit, &p.body).unwrap_or(p.body);
            out.push((commit.clone(), note));
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
            carried_failure: carried_failure_for(commit),
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
    let mut results = verify_notes_once(repo, commits, notes)?;
    check_rebound_sources(repo, &mut results);
    Ok(results)
}

/// [`verify_notes`] without looking at the notes of the commits that
/// rebound notes name.
fn verify_notes_once(
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
        // A stamp naming another commit: what it claims about that commit is
        // checked when the note turns out to be rebound by an older autter.
        if let Some(Ok(integrity)) = &split.integrity
            && integrity.commit != *commit
        {
            push(tree_query(&integrity.commit), &mut queries);
            for path in integrity.files.keys() {
                push(path_query(&integrity.commit, path), &mut queries);
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

    // An unstamped note that passed as legacy (or as a copy git made during
    // a rewrite) on a commit made after stamping began cannot simply be
    // trusted as legacy: see `note_stamp_floor`.
    let unstamped: Vec<String> = results
        .iter()
        .filter(|r| {
            matches!(
                r.status,
                IntegrityStatus::Legacy | IntegrityStatus::PendingRewrite
            )
        })
        .filter(|r| {
            parsed
                .get(r.commit.as_str())
                .is_some_and(|p| p.split.integrity.is_none() && !p.split.integrity_moved)
        })
        .map(|r| r.commit.clone())
        .collect();
    if !unstamped.is_empty() {
        let after = crate::authorship::note_stamp_floor::after_stamping(repo, &unstamped);
        for r in results.iter_mut().filter(|r| after.contains(&r.commit)) {
            let Some(log) = parsed.get(r.commit.as_str()).and_then(|p| p.log.as_ref()) else {
                continue;
            };
            let copied = match (&r.recorded_commit, r.status) {
                (Some(from), IntegrityStatus::PendingRewrite) => {
                    format!(" (git copied it from {} during a rewrite)", short(from))
                }
                _ => String::new(),
            };
            match version_claim(log, FIRST_STAMPING_RELEASE) {
                VersionClaim::Older(version) => {
                    r.status = IntegrityStatus::UnstampedByOlderAutter;
                    r.detail = format!(
                        "the note has no integrity stamp and says autter {version} wrote it{copied}, but its commit comes after commits whose notes are stamped: written by a teammate on an older autter, or its stamp was removed and its version edited"
                    );
                }
                VersionClaim::Unknown | VersionClaim::Stamping(_) => {
                    r.status = IntegrityStatus::StampRemoved;
                    r.detail = format!(
                        "the note has no integrity stamp and no autter version{copied}, but its commit comes after commits whose notes are stamped, so it was not written before stamping began: the stamp was removed"
                    );
                }
            }
        }
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
            let digest_holds = integrity.digest.as_deref()
                == Some(compute_digest(&split.body, integrity).as_str());
            let changed = || {
                result(
                    IntegrityStatus::DigestMismatch,
                    "the note was changed after autter wrote it".to_string(),
                    None,
                )
            };
            let carried = |carried: &CarriedFailure| {
                result(
                    IntegrityStatus::CarriedFailure,
                    format!(
                        "autter derived this note from the note of {}, which failed the check ({}) when autter rewrote it",
                        short(&carried.commit),
                        carried.status
                    ),
                    Some(carried.commit.clone()),
                )
            };
            if integrity.commit != commit {
                if recorded == commit {
                    let rangecheck = range_check(commit, log, objects, line_counts);
                    if let Some(r) = rangecheck {
                        return r;
                    }
                    // An older autter rewrote the text and kept the stamp: a
                    // failure the stamp records still stands.
                    if let Some(c) = &integrity.carried_failure {
                        return carried(c);
                    }
                    // The text changed, so the digest cannot hold. What the
                    // stamp says about the commit it names still can be
                    // checked, when that commit is in this clone.
                    let checked = match stamp_claims_hold(integrity, objects) {
                        Some(Err(detail)) => {
                            return result(
                                IntegrityStatus::ContentMismatch,
                                detail,
                                Some(integrity.commit.clone()),
                            );
                        }
                        Some(Ok(())) => "",
                        None => {
                            " (that commit is not in this clone, so the record could not be checked against it)"
                        }
                    };
                    return result(
                        IntegrityStatus::ReboundByOlderAutter,
                        format!(
                            "an autter release older than the integrity check moved this note from {}{checked}; run a newer autter over the range to stamp it again",
                            short(&integrity.commit)
                        ),
                        Some(integrity.commit.clone()),
                    );
                }
                // Git copies a note byte for byte, so a copy's digest still
                // holds: one that does not was changed, before or after the copy.
                if !digest_holds {
                    return changed();
                }
                let copied = moved(&integrity.commit);
                if copied.status == IntegrityStatus::PendingRewrite
                    && let Some(c) = &integrity.carried_failure
                {
                    return carried(c);
                }
                return copied;
            }
            if !digest_holds {
                return changed();
            }
            // Checked before the commit's files: a carried failure is what
            // the stamp says about this note, whatever else is wrong with it.
            if let Some(c) = &integrity.carried_failure {
                return carried(c);
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
            // Every release since stamping began stamps every note it writes,
            // so a note that says one of them wrote it had its stamp removed.
            if let VersionClaim::Stamping(version) = version_claim(log, FIRST_STAMPING_RELEASE) {
                return result(
                    IntegrityStatus::StampRemoved,
                    format!(
                        "the note has no integrity stamp but says autter {version} wrote it; autter {} and later stamp every note they write, so the stamp was removed",
                        release_name(FIRST_STAMPING_RELEASE)
                    ),
                    None,
                );
            }
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

/// Whether what `integrity` records about the commit it names (its tree and
/// the blob of each file) is true of that commit. `None` when that commit is
/// not in this clone; `Some(Err(detail))` when a claim is false.
fn stamp_claims_hold(
    integrity: &NoteIntegrity,
    objects: &HashMap<String, (String, String)>,
) -> Option<Result<(), String>> {
    let named = &integrity.commit;
    let (_, tree) = objects.get(&tree_query(named))?;
    if *tree != integrity.tree {
        return Some(Err(format!(
            "the note's integrity record names {}, but does not describe that commit's tree",
            short(named)
        )));
    }
    for (path, recorded) in &integrity.files {
        let actual = objects
            .get(&path_query(named, path))
            .filter(|(kind, _)| kind == "blob")
            .map(|(_, oid)| oid.clone());
        if actual != *recorded {
            return Some(Err(format!(
                "the note's integrity record names {}, but describes a different version of {} than that commit has",
                short(named),
                path
            )));
        }
    }
    Some(Ok(()))
}

/// A note rebound by an older autter keeps the stamp of the note it was
/// derived from, and git keeps that note on the commit the stamp names. When
/// that note is still there and fails the check, the rebound note was derived
/// from a failing note: it fails as [`IntegrityStatus::CarriedFailure`], as a
/// rewrite by this autter would have recorded. Never fails: when the notes
/// cannot be read or checked, the results stay as they are.
fn check_rebound_sources(repo: &Repository, results: &mut [CommitIntegrity]) {
    let mut named: Vec<String> = results
        .iter()
        .filter(|r| r.status == IntegrityStatus::ReboundByOlderAutter)
        .filter_map(|r| r.recorded_commit.clone())
        .collect();
    named.sort();
    named.dedup();
    if named.is_empty() {
        return;
    }
    let notes = match crate::git::notes_api::read_notes_batch(repo, &named) {
        Ok(notes) => notes,
        Err(e) => {
            tracing::debug!("note integrity: notes of rebound sources not read: {e}");
            return;
        }
    };
    let with_notes: Vec<String> = named
        .into_iter()
        .filter(|c| notes.contains_key(c))
        .collect();
    if with_notes.is_empty() {
        return;
    }
    let sources = match verify_notes_once(repo, &with_notes, &notes) {
        Ok(sources) => sources,
        Err(e) => {
            tracing::debug!("note integrity: rebound sources not checked: {e}");
            return;
        }
    };
    let failed: HashMap<&str, &CommitIntegrity> = sources
        .iter()
        .filter(|r| r.status.is_failure())
        .map(|r| (r.commit.as_str(), r))
        .collect();
    for r in results.iter_mut() {
        if r.status != IntegrityStatus::ReboundByOlderAutter {
            continue;
        }
        let Some(source) = r
            .recorded_commit
            .as_deref()
            .and_then(|c| failed.get(c).copied())
        else {
            continue;
        };
        r.status = IntegrityStatus::CarriedFailure;
        r.detail = format!(
            "an autter release older than the integrity check moved this note from {}, whose note fails the check ({})",
            short(&source.commit),
            source.status.as_str()
        );
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
            IntegrityStatus::CarriedFailure => "was carried from a note that failed the check",
            IntegrityStatus::ChangedAfterRecorded => "was changed after the server recorded it",
            IntegrityStatus::StampRemoved => "had its integrity stamp removed",
            _ => "was changed after autter wrote it",
        },
        short(&result.commit)
    )
}

// ---------------------------------------------------------------------------
// Rewrites: checking the notes attribution is carried from
// ---------------------------------------------------------------------------

/// The failing source notes of one rewrite, recorded until the rewrite has
/// written its notes. Dropping it forgets them.
#[must_use = "the record lasts only as long as this value"]
pub struct RewriteSourceCheck {
    commits: Vec<String>,
}

impl Drop for RewriteSourceCheck {
    fn drop(&mut self) {
        if self.commits.is_empty() {
            return;
        }
        if let Ok(mut map) = carried_failures().lock() {
            for commit in &self.commits {
                map.remove(commit);
            }
        }
    }
}

/// Check the notes a rewrite carries attribution from, before it writes the
/// new commits' notes. `derived_from` lists each new commit with the source
/// commits its note is built from (a squash has several). `source_notes`
/// holds the source notes when the caller already read them (commit -> raw
/// note, every source with a note); otherwise they are read here.
///
/// All sources are checked in one pass ([`verify_notes`]: a fixed number of
/// git processes however many commits the rewrite has). A new commit with a
/// source that fails is recorded, and its note is stamped with
/// `carried_failure` naming that source when the rewrite writes it, so it
/// fails `autter verify` instead of being stamped valid.
///
/// Sources that are `verified`, `legacy`, `missing`, `pending_rewrite` (git
/// copied the note during this very rebase or amend) or
/// `rebound_by_older_autter` are carried and stamped as before. So is a
/// source that is `unavailable`: its commit or files are not in this clone
/// (a shallow clone), so it could be checked only partly, and nothing found
/// shows it was changed. If the check itself cannot run, the rewrite carries
/// the notes as before, as readers do.
pub fn check_rewrite_sources(
    repo: &Repository,
    derived_from: &[(String, Vec<String>)],
    source_notes: Option<&HashMap<String, String>>,
) -> RewriteSourceCheck {
    let mut check = RewriteSourceCheck {
        commits: Vec::new(),
    };
    let mut sources: Vec<String> = derived_from
        .iter()
        .flat_map(|(_, sources)| sources.iter().cloned())
        .collect();
    sources.sort();
    sources.dedup();
    if sources.is_empty() {
        return check;
    }
    let read;
    let notes = match source_notes {
        Some(notes) => notes,
        None => match crate::git::notes_api::read_notes_batch(repo, &sources) {
            Ok(notes) => {
                read = notes;
                &read
            }
            Err(e) => {
                tracing::debug!("note integrity: rewrite sources not checked: {e}");
                return check;
            }
        },
    };
    let with_notes: Vec<String> = sources
        .into_iter()
        .filter(|commit| notes.contains_key(commit))
        .collect();
    if with_notes.is_empty() {
        return check;
    }
    let results = match verify_notes(repo, &with_notes, notes) {
        Ok(results) => results,
        Err(e) => {
            tracing::debug!("note integrity: rewrite sources not checked: {e}");
            return check;
        }
    };
    let failed: HashMap<&str, &CommitIntegrity> = results
        .iter()
        .filter(|r| r.status.is_failure())
        .map(|r| (r.commit.as_str(), r))
        .collect();
    if failed.is_empty() {
        return check;
    }
    let Ok(mut map) = carried_failures().lock() else {
        return check;
    };
    for (new_commit, sources) in derived_from {
        let Some(source) = sources.iter().find_map(|s| failed.get(s.as_str())) else {
            continue;
        };
        tracing::debug!(
            "note integrity: {} is derived from the failing note of {} ({}); its note will say so",
            new_commit,
            source.commit,
            source.status.as_str()
        );
        map.insert(
            new_commit.clone(),
            CarriedFailure {
                commit: source.commit.clone(),
                status: source.status.as_str().to_string(),
            },
        );
        check.commits.push(new_commit.clone());
    }
    check
}

// ---------------------------------------------------------------------------
// Stash notes (refs/notes/ai-stash)
// ---------------------------------------------------------------------------
//
// `git stash` saves the working log's attribution as a note on the stash
// commit, and `stash pop`/`apply` puts it back into the working log, where
// the next commit stamps it. A stash note is bound to its stash commit the
// same way a note is bound to its commit: `commit` is the stash commit,
// `tree` its tree, `files` the blob of each attested file in the stash (the
// stash commit's tree, or its untracked-files parent for `stash -u`), and
// the digest covers the note.

/// The places a stashed file's content is: the stash commit's tree (tracked
/// files), then its third parent (untracked files, `git stash -u`).
fn stash_path_queries(stash_sha: &str, path: &str) -> [String; 2] {
    [
        path_query(stash_sha, path),
        path_query(&format!("{stash_sha}^3"), path),
    ]
}

fn stash_blob(
    objects: &HashMap<String, (String, String)>,
    stash_sha: &str,
    path: &str,
) -> Option<String> {
    stash_path_queries(stash_sha, path).iter().find_map(|q| {
        objects
            .get(q)
            .filter(|(kind, _)| kind == "blob")
            .map(|(_, oid)| oid.clone())
    })
}

fn stash_objects(
    repo: &Repository,
    stash_sha: &str,
    log: &AuthorshipLog,
    extra_paths: impl Iterator<Item = String>,
) -> Result<HashMap<String, (String, String)>, AutterError> {
    let mut queries = vec![tree_query(stash_sha)];
    let mut seen = HashSet::new();
    for path in log
        .attestations
        .iter()
        .map(|a| a.file_path.clone())
        .chain(extra_paths)
    {
        if seen.insert(path.clone()) {
            queries.extend(stash_path_queries(stash_sha, &path));
        }
    }
    batch_check(repo, &queries)
}

/// `note` (a stash's authorship log) stamped for the stash commit
/// `stash_sha`. Never fails the stash: on any error the note is returned
/// as it is, and it restores as a note written before stash notes were
/// stamped.
pub fn seal_stash_note(repo: &Repository, stash_sha: &str, note: &str) -> String {
    let body = split_note(note).body;
    let Ok(log) = AuthorshipLog::deserialize_from_string(&body) else {
        return note.to_string();
    };
    let objects = match stash_objects(repo, stash_sha, &log, std::iter::empty()) {
        Ok(objects) => objects,
        Err(e) => {
            tracing::debug!("note integrity: stash note not stamped: {e}");
            return note.to_string();
        }
    };
    let Some((_, tree)) = objects.get(&tree_query(stash_sha)) else {
        return note.to_string();
    };
    let files = log
        .attestations
        .iter()
        .map(|a| {
            (
                a.file_path.clone(),
                stash_blob(&objects, stash_sha, &a.file_path),
            )
        })
        .collect();
    let integrity = NoteIntegrity {
        v: INTEGRITY_FORMAT_VERSION,
        commit: stash_sha.to_string(),
        tree: tree.clone(),
        files,
        carried_failure: None,
        digest: None,
    };
    insert_integrity(&body, &integrity).unwrap_or_else(|| note.to_string())
}

/// Result of checking a stash note before restoring it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashNoteCheck {
    /// Stamped for this stash and unchanged since.
    Verified,
    /// No integrity member: written by an autter older than stash-note
    /// stamping (3.8.1 and earlier). Restored as before. A note with its
    /// member removed is not this but `Failed` when its metadata names a
    /// release that stamps stash notes, or has no version and the stash was
    /// made after stamping began; one whose version was also edited to an
    /// older release still reads as this.
    Legacy,
    /// The note does not belong to this stash or was changed after autter
    /// wrote it; the reason completes "its record ...".
    Failed(String),
}

/// Check the stash note `note` of the stash commit `stash_sha` before its
/// attribution is restored into the working log.
pub fn check_stash_note(repo: &Repository, stash_sha: &str, note: &str) -> StashNoteCheck {
    let split = split_note(note);
    if split.integrity_moved {
        return StashNoteCheck::Failed(
            "has an integrity record that was moved or reformatted after autter wrote it"
                .to_string(),
        );
    }
    let integrity = match split.integrity {
        None => return check_unstamped_stash_note(repo, stash_sha, &split.body),
        Some(Err(e)) => {
            return StashNoteCheck::Failed(format!("has an unreadable integrity record ({e})"));
        }
        Some(Ok(integrity)) => integrity,
    };
    if integrity.commit != stash_sha {
        return StashNoteCheck::Failed(format!(
            "was written for {}, not for this stash",
            short(&integrity.commit)
        ));
    }
    if integrity.digest.as_deref() != Some(compute_digest(&split.body, &integrity).as_str()) {
        return StashNoteCheck::Failed("was changed after autter wrote it".to_string());
    }
    if integrity.carried_failure.is_some() {
        return StashNoteCheck::Failed("was derived from a note that failed the check".to_string());
    }
    let Ok(log) = AuthorshipLog::deserialize_from_string(&split.body) else {
        return StashNoteCheck::Failed("is not a readable authorship record".to_string());
    };
    let objects = match stash_objects(repo, stash_sha, &log, integrity.files.keys().cloned()) {
        Ok(objects) => objects,
        // The digest holds; only the comparison with the stash could not run.
        Err(e) => {
            tracing::debug!("note integrity: stash note compared by digest only: {e}");
            return StashNoteCheck::Verified;
        }
    };
    match objects.get(&tree_query(stash_sha)) {
        Some((_, tree)) if *tree == integrity.tree => {}
        _ => {
            return StashNoteCheck::Failed("does not describe this stash's tree".to_string());
        }
    }
    for a in &log.attestations {
        match integrity.files.get(&a.file_path) {
            Some(recorded) if *recorded == stash_blob(&objects, stash_sha, &a.file_path) => {}
            Some(_) => {
                return StashNoteCheck::Failed(format!(
                    "describes a different version of {} than the stash has",
                    a.file_path
                ));
            }
            None => {
                return StashNoteCheck::Failed(format!(
                    "attributes lines in {}, which its integrity record does not cover",
                    a.file_path
                ));
            }
        }
    }
    StashNoteCheck::Verified
}

/// A stash note without a stamp is restored as one written before stash
/// notes were stamped (3.8.1 and earlier), unless that cannot be: its
/// metadata says a release that stamps stash notes wrote it, or it has no
/// readable autter version and the commit the stash was made on comes at or
/// after a commit whose note is stamped (`note_stamp_floor`). A note that
/// says an older release wrote it is restored: 3.8.0 and 3.8.1 stamped
/// commit notes but not stash notes, so such a stash on a stamped commit is
/// ordinary.
fn check_unstamped_stash_note(repo: &Repository, stash_sha: &str, body: &str) -> StashNoteCheck {
    let Ok(log) = AuthorshipLog::deserialize_from_string(body) else {
        return StashNoteCheck::Legacy;
    };
    match version_claim(&log, FIRST_STASH_STAMPING_RELEASE) {
        VersionClaim::Older(_) => StashNoteCheck::Legacy,
        VersionClaim::Stamping(version) => StashNoteCheck::Failed(format!(
            "has no integrity stamp but says autter {version} wrote it; autter {} and later stamp every stash note, so the stamp was removed",
            release_name(FIRST_STASH_STAMPING_RELEASE)
        )),
        VersionClaim::Unknown => {
            let base_query = format!("{stash_sha}^1");
            let base = batch_check(repo, std::slice::from_ref(&base_query))
                .ok()
                .and_then(|found| found.get(&base_query).map(|(_, oid)| oid.clone()));
            match base {
                Some(base)
                    if crate::authorship::note_stamp_floor::at_or_after_stamping(
                        repo, &base,
                    ) =>
                {
                    StashNoteCheck::Failed(
                        "has no integrity stamp and no autter version, but the stash was made on a commit after stamping began, so the stamp was removed"
                            .to_string(),
                    )
                }
                _ => StashNoteCheck::Legacy,
            }
        }
    }
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
            carried_failure: None,
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
    fn note_written_for_reads_the_stamp_then_the_base_commit() {
        // Legacy note: base_commit_sha decides.
        assert!(note_written_for(NOTE, "c0ffee"));
        assert!(!note_written_for(NOTE, "beef"));
        // Stamped note: the stamp decides, also when base_commit_sha differs.
        let stamped = insert_integrity(
            NOTE,
            &NoteIntegrity {
                v: INTEGRITY_FORMAT_VERSION,
                commit: "beef".to_string(),
                tree: "7ree".to_string(),
                files: BTreeMap::new(),
                carried_failure: None,
                digest: None,
            },
        )
        .unwrap();
        assert!(note_written_for(&stamped, "beef"));
        assert!(!note_written_for(&stamped, "c0ffee"));
        assert!(!note_written_for("not a note", "c0ffee"));
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
    fn a_carried_failure_is_covered_by_the_digest() {
        // Notes stamped without the field serialize as before, so their
        // digests still hold.
        let fields = serde_json::to_string(&integrity()).unwrap();
        assert!(!fields.contains("carried_failure"), "{fields}");
        let mut carried = integrity();
        carried.carried_failure = Some(CarriedFailure {
            commit: "bad".to_string(),
            status: "digest_mismatch".to_string(),
        });
        assert_ne!(
            compute_digest(NOTE, &integrity()),
            compute_digest(NOTE, &carried)
        );
        let stamped = insert_integrity(NOTE, &carried).unwrap();
        let parsed = split_note(&stamped).integrity.unwrap().unwrap();
        assert_eq!(parsed.carried_failure, carried.carried_failure);
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
            CarriedFailure,
            ChangedAfterRecorded,
            NotRecorded,
            StampRemoved,
            UnstampedByOlderAutter,
        ] {
            assert!(!(s.is_failure() && s.is_warning()), "{}", s.as_str());
            assert_eq!(s == Verified, !s.is_failure() && !s.is_warning());
        }
        assert!(StampRemoved.is_failure());
        assert!(UnstampedByOlderAutter.is_warning());
    }

    fn log_with_version(version: Option<&str>) -> AuthorshipLog {
        let mut log = AuthorshipLog::deserialize_from_string(NOTE).unwrap();
        log.metadata.autter_version = version.map(str::to_string);
        log
    }

    #[test]
    fn version_claims_compare_with_the_first_stamping_release() {
        let claim = |v: Option<&str>| version_claim(&log_with_version(v), FIRST_STAMPING_RELEASE);
        assert_eq!(
            claim(Some("3.9.1")),
            VersionClaim::Stamping("3.9.1".to_string())
        );
        assert_eq!(
            claim(Some("3.8.0")),
            VersionClaim::Stamping("3.8.0".to_string())
        );
        assert_eq!(
            claim(Some("development:3.9.1")),
            VersionClaim::Stamping("development:3.9.1".to_string())
        );
        assert_eq!(
            claim(Some("4.0.0-rc.1")),
            VersionClaim::Stamping("4.0.0-rc.1".to_string())
        );
        assert_eq!(
            claim(Some("3.7.1")),
            VersionClaim::Older("3.7.1".to_string())
        );
        assert_eq!(
            claim(Some("2.4.0")),
            VersionClaim::Older("2.4.0".to_string())
        );
        assert_eq!(claim(Some("development")), VersionClaim::Unknown);
        assert_eq!(claim(Some("")), VersionClaim::Unknown);
        assert_eq!(claim(Some("3.x")), VersionClaim::Unknown);
        assert_eq!(claim(None), VersionClaim::Unknown);
        // Stash notes were stamped from 3.8.2.
        let stash =
            |v: &str| version_claim(&log_with_version(Some(v)), FIRST_STASH_STAMPING_RELEASE);
        assert_eq!(stash("3.8.1"), VersionClaim::Older("3.8.1".to_string()));
        assert_eq!(stash("3.8.2"), VersionClaim::Stamping("3.8.2".to_string()));
    }
}
