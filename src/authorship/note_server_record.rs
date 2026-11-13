//! Comparing authorship notes with the server's record of them.
//!
//! The integrity check (`note_integrity`) catches notes copied to another
//! commit or edited by hand, but anyone who can write the repository can edit
//! a note and recompute its digest. In connected mode the server keeps every
//! version of each note it receives (append-only, the first version kept), so
//! `autter verify` can compare a note with that record: a note edited after it
//! was uploaded, even by its author, no longer matches the first version, and
//! the record shows how many versions there have been.
//!
//! **Matching.** A local note matches a recorded version by
//! `integrity.digest` when both have one and the local digest was checked
//! (the note is `verified`), otherwise by [`note_content_sha256`] of the note
//! text. Two recorded versions are the same when their digests are equal, or
//! (when either has none) when their content hashes are.
//!
//! **Outcomes** ([`compare`]): the note matches the first and only version
//! ([`RecordOutcome::Matches`]); the record has more than one version, or the
//! note matches none of them, or only a later one
//! ([`RecordOutcome::Changed`], status `changed_after_recorded`, a failure);
//! the note exists but the server has no record of it yet
//! ([`RecordOutcome::NotRecorded`], status `not_recorded`, a warning, since
//! notes upload in the background); no note here but the server recorded one
//! ([`RecordOutcome::RecordedButMissing`], status stays `missing`).
//!
//! **Notes read from the server** (the `http` notes backend, no local git
//! notes): the note in hand came from the server, so only "more than one
//! version" is checked ([`CompareMode::VersionsOnly`]).
//!
//! **What it does not catch.** A note that was never uploaded (local-only
//! mode, or not uploaded yet) has nothing to compare with; local-only mode is
//! still only an integrity check. The record starts at the first upload the
//! server saw (or, for `seeded` versions, at the server's upgrade).

use crate::api::client::{ApiClient, ApiContext};
use crate::api::notes::MAX_RECORD_COMMITS;
use crate::api::types::{NoteRecord, NoteRecordVersion};
use crate::authorship::note_integrity::{CommitIntegrity, IntegrityStatus, read_integrity};
use crate::config::{Config, NotesBackendKind};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// SHA-256 (hex) of a note's text with trailing whitespace (space, tab, CR,
/// LF) trimmed: the hash the server records as `content_sha256`.
pub fn note_content_sha256(note: &str) -> String {
    let trimmed = note.trim_end_matches([' ', '\t', '\r', '\n']);
    format!("{:x}", Sha256::digest(trimmed.as_bytes()))
}

/// How a local note is compared with the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareMode {
    /// The note was read from this repository: compare it with each version.
    Content,
    /// The note was read from the server (the `http` notes backend): only
    /// check whether the record has more than one version.
    VersionsOnly,
}

/// What comparing one commit's note with the record found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// The note matches the first version, and the record has no other.
    Matches,
    /// The record has more than one version, or the note matches none of
    /// them or only a later one. `matched_version` is the 1-based index of
    /// the first version the note matches, if any.
    Changed { matched_version: Option<usize> },
    /// The commit has a note but the server has no record of it.
    NotRecorded,
    /// The commit has no note here, but the server recorded one.
    RecordedButMissing,
    /// No note and no record.
    Nothing,
}

/// What the record says about a commit, for the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordSummary {
    /// Distinct versions on record.
    pub versions: usize,
    /// When the server received the first version.
    pub first_recorded_at: String,
    /// Who uploaded the first version, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_uploaded_by: Option<String>,
    /// The first version was copied in when the server started keeping
    /// versions: the earliest the server knows, not necessarily the original.
    pub first_seeded: bool,
    /// The 1-based version the note here matches, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matches_version: Option<usize>,
}

/// One commit's comparison with the record, as `autter verify` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordComparison {
    /// `matches`, `changed`, `not_recorded` or `recorded` (no note here, but
    /// the server has one).
    pub record: &'static str,
    /// The integrity status before the record changed it, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_status: Option<IntegrityStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_summary: Option<RecordSummary>,
}

fn same_version(a: &NoteRecordVersion, b: &NoteRecordVersion) -> bool {
    match (&a.integrity_digest, &b.integrity_digest) {
        (Some(da), Some(db)) => da.eq_ignore_ascii_case(db),
        _ => a.content_sha256.eq_ignore_ascii_case(&b.content_sha256),
    }
}

/// Number of distinct versions in `versions`.
fn distinct_versions(versions: &[NoteRecordVersion]) -> usize {
    let mut reps: Vec<&NoteRecordVersion> = Vec::new();
    for v in versions {
        if !reps.iter().any(|r| same_version(r, v)) {
            reps.push(v);
        }
    }
    reps.len()
}

/// Compare `local_note` (the commit's note here, if any) with `versions`
/// (the server's record, oldest first; empty when there is none).
/// `digest_checked`: the note's digest was verified locally, so it can stand
/// for the note's content.
pub fn compare(
    local_note: Option<&str>,
    versions: &[NoteRecordVersion],
    mode: CompareMode,
    digest_checked: bool,
) -> RecordOutcome {
    let Some(note) = local_note else {
        return if versions.is_empty() {
            RecordOutcome::Nothing
        } else {
            RecordOutcome::RecordedButMissing
        };
    };
    if versions.is_empty() {
        return RecordOutcome::NotRecorded;
    }
    let distinct = distinct_versions(versions);
    if mode == CompareMode::VersionsOnly {
        return if distinct == 1 {
            RecordOutcome::Matches
        } else {
            RecordOutcome::Changed {
                matched_version: None,
            }
        };
    }

    let local_digest = if digest_checked {
        read_integrity(note).and_then(|i| i.digest)
    } else {
        None
    };
    let local_sha = note_content_sha256(note);
    let matches = |v: &NoteRecordVersion| {
        if let (Some(ld), Some(vd)) = (&local_digest, &v.integrity_digest) {
            return ld.eq_ignore_ascii_case(vd);
        }
        local_sha.eq_ignore_ascii_case(&v.content_sha256)
    };
    let matched_version = versions.iter().position(matches).map(|i| i + 1);
    if distinct == 1 && matched_version.is_some() {
        RecordOutcome::Matches
    } else {
        RecordOutcome::Changed { matched_version }
    }
}

fn summary(versions: &[NoteRecordVersion], matches_version: Option<usize>) -> RecordSummary {
    let first = &versions[0];
    RecordSummary {
        versions: distinct_versions(versions),
        first_recorded_at: first.recorded_at.clone(),
        first_uploaded_by: first.uploaded_by.clone(),
        first_seeded: first.seeded,
        matches_version,
    }
}

/// "first recorded <when> by <who>", or the seeded wording.
fn first_recorded(s: &RecordSummary) -> String {
    let who = s
        .first_uploaded_by
        .as_deref()
        .unwrap_or("an unknown uploader");
    if s.first_seeded {
        format!(
            "the record starts at the server's upgrade on {} (uploaded by {who}; the earliest version it has, not necessarily the original)",
            s.first_recorded_at
        )
    } else {
        format!("first recorded {} by {who}", s.first_recorded_at)
    }
}

fn changed_detail(s: &RecordSummary, mode: CompareMode) -> String {
    let count = if s.versions == 1 {
        "1 version".to_string()
    } else {
        format!("{} versions", s.versions)
    };
    let matched = match (mode, s.matches_version) {
        (CompareMode::VersionsOnly, _) => String::new(),
        (_, Some(1)) => "; this note matches the first".to_string(),
        (_, Some(k)) => format!("; this note matches version {k}"),
        (_, None) => "; this note matches none of them".to_string(),
    };
    format!(
        "the note was changed after the server recorded it: {count} on record, {}{matched}",
        first_recorded(s)
    )
}

/// Compare `result` (a commit's integrity check) with the record and update
/// it: `changed_after_recorded` and `not_recorded` replace a status that did
/// not fail locally; a local failure stays as it is. Returns what the record
/// says, or `None` when there is neither a note nor a record.
pub fn apply_record(
    result: &mut CommitIntegrity,
    local_note: Option<&str>,
    record: Option<&NoteRecord>,
    mode: CompareMode,
) -> Option<RecordComparison> {
    let versions: &[NoteRecordVersion] = record.map(|r| r.versions.as_slice()).unwrap_or(&[]);
    let local_note = local_note.filter(|_| result.status != IntegrityStatus::Missing);
    let digest_checked = result.status == IntegrityStatus::Verified;
    let outcome = compare(local_note, versions, mode, digest_checked);
    let replaceable = !result.status.is_failure() && result.status != IntegrityStatus::Missing;
    let mut local_status = None;
    let mut replace = |result: &mut CommitIntegrity, status: IntegrityStatus, detail: String| {
        if replaceable {
            local_status = Some(result.status);
            result.status = status;
            result.detail = detail;
        }
    };
    let (record, record_summary) = match outcome {
        RecordOutcome::Nothing => return None,
        RecordOutcome::Matches => ("matches", Some(summary(versions, Some(1)))),
        RecordOutcome::Changed { matched_version } => {
            let s = summary(versions, matched_version);
            replace(
                result,
                IntegrityStatus::ChangedAfterRecorded,
                changed_detail(&s, mode),
            );
            ("changed", Some(s))
        }
        RecordOutcome::NotRecorded => {
            let detail = format!(
                "{}; the server has no record of this note yet (notes upload in the background, so a new commit may not be recorded yet)",
                result.detail
            );
            replace(result, IntegrityStatus::NotRecorded, detail);
            ("not_recorded", None)
        }
        RecordOutcome::RecordedButMissing => {
            let s = summary(versions, None);
            result.detail = format!(
                "{}; the server recorded a note for this commit ({}): it was deleted here, or not fetched (`autter fetch-notes`)",
                result.detail,
                first_recorded(&s)
            );
            ("recorded", Some(s))
        }
    };
    Some(RecordComparison {
        record,
        local_status,
        record_summary,
    })
}

/// The server's record for a set of commits.
#[derive(Debug)]
pub struct FetchedRecords {
    pub records: HashMap<String, NoteRecord>,
    pub mode: CompareMode,
}

/// Why the server's record could not be checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordUnavailable {
    /// A short reason (local mode, not logged in, server unreachable ...).
    pub reason: String,
    /// The machine is in connected mode (notes are synced), so the record
    /// was expected and its absence is worth telling the user about.
    pub connected: bool,
}

impl RecordUnavailable {
    fn local(reason: &str) -> Self {
        RecordUnavailable {
            reason: reason.to_string(),
            connected: false,
        }
    }

    fn connected(reason: String) -> Self {
        RecordUnavailable {
            reason,
            connected: true,
        }
    }
}

/// Read the server's record of `commits`' notes, in chunks of
/// [`MAX_RECORD_COMMITS`]. `Err` says why the record could not be checked
/// (local mode, not logged in, server unreachable or too old); none of these
/// is a failure by itself.
pub fn fetch_records(commits: &[String]) -> Result<FetchedRecords, RecordUnavailable> {
    let cfg = Config::fresh();
    if !cfg.platform_sync_enabled() {
        return Err(RecordUnavailable::local(
            "local mode: notes are not uploaded to the server",
        ));
    }
    let Some(url) = cfg.notes_backend_url() else {
        return Err(RecordUnavailable::local("no notes server is configured"));
    };
    let mode = if cfg.notes_backend_kind() == NotesBackendKind::Http {
        CompareMode::VersionsOnly
    } else {
        CompareMode::Content
    };
    let client = ApiClient::new(ApiContext::new(Some(url.to_string())).with_timeout(15));
    if !client.is_logged_in() && !client.has_api_key() {
        return Err(RecordUnavailable::connected(
            "not logged in (run `autter login`)".to_string(),
        ));
    }
    let mut records = HashMap::new();
    for chunk in commits.chunks(MAX_RECORD_COMMITS) {
        let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
        match client.read_note_records(&refs) {
            Ok(Some(response)) => records.extend(response.records),
            Ok(None) => {
                return Err(RecordUnavailable::connected(
                    "the server does not keep a record of note versions".to_string(),
                ));
            }
            Err(e) => {
                return Err(RecordUnavailable::connected(format!(
                    "the server could not be read: {e}"
                )));
            }
        }
    }
    Ok(FetchedRecords { records, mode })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::note_integrity::{NoteIntegrity, seal_note};
    use std::collections::BTreeMap;

    const NOTE_BODY: &str = "src/a.rs\n  abcd1234abcd1234 1-2\n---\n{\n  \"schema_version\": \"authorship/3.0.0\",\n  \"base_commit_sha\": \"c0ffee\",\n  \"prompts\": {}\n}";

    fn sealed(body: &str) -> String {
        seal_note(
            body,
            &NoteIntegrity {
                v: 1,
                commit: "c0ffee".to_string(),
                tree: "7ree".to_string(),
                files: BTreeMap::new(),
                carried_failure: None,
                digest: None,
            },
        )
        .unwrap()
    }

    fn version_of(note: &str, at: &str) -> NoteRecordVersion {
        NoteRecordVersion {
            content_sha256: note_content_sha256(note),
            integrity_digest: read_integrity(note).and_then(|i| i.digest),
            uploaded_by: Some("user_1".to_string()),
            recorded_at: at.to_string(),
            seeded: false,
        }
    }

    fn integrity(status: IntegrityStatus) -> CommitIntegrity {
        CommitIntegrity {
            commit: "c0ffee".to_string(),
            status,
            detail: "local detail".to_string(),
            recorded_commit: None,
        }
    }

    #[test]
    fn content_sha_trims_trailing_whitespace_only() {
        assert_eq!(note_content_sha256("abc\n"), note_content_sha256("abc"));
        assert_eq!(
            note_content_sha256("abc \t\r\n\n"),
            note_content_sha256("abc")
        );
        assert_ne!(note_content_sha256(" abc"), note_content_sha256("abc"));
        // Only space, tab, CR and LF are trimmed (the server's rule), not
        // other whitespace such as a vertical tab or a no-break space.
        assert_ne!(note_content_sha256("abc\u{0b}"), note_content_sha256("abc"));
        assert_ne!(note_content_sha256("abc\u{a0}"), note_content_sha256("abc"));
        assert_eq!(
            note_content_sha256("abc\u{0b}\n"),
            note_content_sha256("abc\u{0b}")
        );
        assert_eq!(
            note_content_sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Lowercase hex, like the server.
        assert_eq!(
            note_content_sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn server_version_without_digest_falls_back_to_content() {
        // A seeded row whose digest the server could not extract (a
        // reformatted note): match by content even though the note has one.
        let note = sealed(NOTE_BODY);
        let mut v = version_of(&note, "2026-11-01T00:00:00Z");
        v.integrity_digest = None;
        v.seeded = true;
        assert_eq!(
            compare(Some(&note), &[v.clone()], CompareMode::Content, true),
            RecordOutcome::Matches
        );
        let edited = sealed(&NOTE_BODY.replace("1-2", "1-9"));
        assert_eq!(
            compare(Some(&edited), &[v], CompareMode::Content, true),
            RecordOutcome::Changed {
                matched_version: None
            }
        );
    }

    #[test]
    fn note_matching_the_only_version_matches() {
        let note = sealed(NOTE_BODY);
        let v = [version_of(&note, "2026-11-01T00:00:00Z")];
        assert_eq!(
            compare(Some(&note), &v, CompareMode::Content, true),
            RecordOutcome::Matches
        );
        // A trailing newline (git notes adds one) does not matter.
        let with_newline = format!("{note}\n");
        assert_eq!(
            compare(Some(&with_newline), &v, CompareMode::Content, false),
            RecordOutcome::Matches
        );
    }

    #[test]
    fn later_versions_with_the_same_digest_are_one_version() {
        let note = sealed(NOTE_BODY);
        let mut again = version_of(&note, "2026-11-02T00:00:00Z");
        again.content_sha256 = "different-whitespace".to_string();
        let v = [version_of(&note, "2026-11-01T00:00:00Z"), again];
        assert_eq!(
            compare(Some(&note), &v, CompareMode::Content, true),
            RecordOutcome::Matches
        );
    }

    #[test]
    fn edit_with_recomputed_digest_is_changed() {
        let original = sealed(NOTE_BODY);
        let edited = sealed(&NOTE_BODY.replace("1-2", "1-9"));
        let v = [version_of(&original, "2026-11-01T00:00:00Z")];
        assert_eq!(
            compare(Some(&edited), &v, CompareMode::Content, true),
            RecordOutcome::Changed {
                matched_version: None
            }
        );
    }

    #[test]
    fn note_matching_a_later_version_is_changed() {
        let original = sealed(NOTE_BODY);
        let edited = sealed(&NOTE_BODY.replace("1-2", "1-9"));
        let v = [
            version_of(&original, "2026-11-01T00:00:00Z"),
            version_of(&edited, "2026-11-03T00:00:00Z"),
        ];
        assert_eq!(
            compare(Some(&edited), &v, CompareMode::Content, true),
            RecordOutcome::Changed {
                matched_version: Some(2)
            }
        );
        // Even the original fails while the record has two versions.
        assert_eq!(
            compare(Some(&original), &v, CompareMode::Content, true),
            RecordOutcome::Changed {
                matched_version: Some(1)
            }
        );
    }

    #[test]
    fn unchecked_digest_does_not_stand_for_the_content() {
        // A note whose digest was not verified (say, rebound by an older
        // autter) keeps a stale digest after an edit: match by content.
        let original = sealed(NOTE_BODY);
        let integrity_line = original
            .lines()
            .find(|l| l.starts_with("  \"integrity\""))
            .unwrap()
            .to_string();
        let edited = original.replace("1-2", "1-9");
        assert!(edited.contains(&integrity_line));
        let v = [version_of(&original, "2026-11-01T00:00:00Z")];
        assert_eq!(
            compare(Some(&edited), &v, CompareMode::Content, false),
            RecordOutcome::Changed {
                matched_version: None
            }
        );
    }

    #[test]
    fn legacy_note_without_digest_matches_by_content() {
        let v = [NoteRecordVersion {
            content_sha256: note_content_sha256(NOTE_BODY),
            integrity_digest: None,
            uploaded_by: None,
            recorded_at: "2026-01-01T00:00:00Z".to_string(),
            seeded: true,
        }];
        assert_eq!(
            compare(Some(NOTE_BODY), &v, CompareMode::Content, false),
            RecordOutcome::Matches
        );
        let edited = NOTE_BODY.replace("1-2", "1-9");
        assert_eq!(
            compare(Some(&edited), &v, CompareMode::Content, false),
            RecordOutcome::Changed {
                matched_version: None
            }
        );
    }

    #[test]
    fn no_record_and_no_note() {
        let note = sealed(NOTE_BODY);
        assert_eq!(
            compare(Some(&note), &[], CompareMode::Content, true),
            RecordOutcome::NotRecorded
        );
        let v = [version_of(&note, "2026-11-01T00:00:00Z")];
        assert_eq!(
            compare(None, &v, CompareMode::Content, false),
            RecordOutcome::RecordedButMissing
        );
        assert_eq!(
            compare(None, &[], CompareMode::Content, false),
            RecordOutcome::Nothing
        );
    }

    #[test]
    fn versions_only_mode_counts_versions() {
        let original = sealed(NOTE_BODY);
        let edited = sealed(&NOTE_BODY.replace("1-2", "1-9"));
        let one = [version_of(&original, "2026-11-01T00:00:00Z")];
        // The note in hand is ignored: it came from the server.
        assert_eq!(
            compare(Some(&edited), &one, CompareMode::VersionsOnly, true),
            RecordOutcome::Matches
        );
        let two = [
            version_of(&original, "2026-11-01T00:00:00Z"),
            version_of(&edited, "2026-11-02T00:00:00Z"),
        ];
        assert_eq!(
            compare(Some(&edited), &two, CompareMode::VersionsOnly, true),
            RecordOutcome::Changed {
                matched_version: None
            }
        );
    }

    #[test]
    fn apply_record_replaces_passing_status_with_changed() {
        let original = sealed(NOTE_BODY);
        let edited = sealed(&NOTE_BODY.replace("1-2", "1-9"));
        let record = NoteRecord {
            versions: vec![
                version_of(&original, "2026-11-01T00:00:00Z"),
                version_of(&edited, "2026-11-02T00:00:00Z"),
            ],
        };
        let mut r = integrity(IntegrityStatus::Verified);
        let cmp = apply_record(&mut r, Some(&edited), Some(&record), CompareMode::Content).unwrap();
        assert_eq!(r.status, IntegrityStatus::ChangedAfterRecorded);
        assert!(r.status.is_failure());
        assert_eq!(cmp.record, "changed");
        assert_eq!(cmp.local_status, Some(IntegrityStatus::Verified));
        let s = cmp.record_summary.unwrap();
        assert_eq!(s.versions, 2);
        assert_eq!(s.matches_version, Some(2));
        assert!(r.detail.contains("2 versions on record"), "{}", r.detail);
        assert!(
            r.detail
                .contains("first recorded 2026-11-01T00:00:00Z by user_1"),
            "{}",
            r.detail
        );
        assert!(r.detail.contains("matches version 2"), "{}", r.detail);
    }

    #[test]
    fn apply_record_keeps_local_failures() {
        let original = sealed(NOTE_BODY);
        let record = NoteRecord {
            versions: vec![version_of(&original, "2026-11-01T00:00:00Z")],
        };
        let mut r = integrity(IntegrityStatus::DigestMismatch);
        let cmp =
            apply_record(&mut r, Some("edited"), Some(&record), CompareMode::Content).unwrap();
        assert_eq!(r.status, IntegrityStatus::DigestMismatch);
        assert_eq!(r.detail, "local detail");
        assert_eq!(cmp.record, "changed");
        assert_eq!(cmp.local_status, None);
    }

    #[test]
    fn apply_record_legacy_and_pending_become_changed() {
        let record = NoteRecord {
            versions: vec![version_of(NOTE_BODY, "2026-11-01T00:00:00Z")],
        };
        for status in [IntegrityStatus::Legacy, IntegrityStatus::PendingRewrite] {
            let mut r = integrity(status);
            let cmp =
                apply_record(&mut r, Some("other"), Some(&record), CompareMode::Content).unwrap();
            assert_eq!(r.status, IntegrityStatus::ChangedAfterRecorded);
            assert_eq!(cmp.local_status, Some(status));
        }
    }

    #[test]
    fn apply_record_match_leaves_status() {
        let note = sealed(NOTE_BODY);
        let record = NoteRecord {
            versions: vec![version_of(&note, "2026-11-01T00:00:00Z")],
        };
        let mut r = integrity(IntegrityStatus::Verified);
        let cmp = apply_record(&mut r, Some(&note), Some(&record), CompareMode::Content).unwrap();
        assert_eq!(r.status, IntegrityStatus::Verified);
        assert_eq!(r.detail, "local detail");
        assert_eq!(cmp.record, "matches");
        assert_eq!(cmp.local_status, None);
    }

    #[test]
    fn apply_record_not_recorded_is_a_warning() {
        let note = sealed(NOTE_BODY);
        let mut r = integrity(IntegrityStatus::Verified);
        let cmp = apply_record(&mut r, Some(&note), None, CompareMode::Content).unwrap();
        assert_eq!(r.status, IntegrityStatus::NotRecorded);
        assert!(r.status.is_warning());
        assert!(!r.status.is_failure());
        assert_eq!(cmp.record, "not_recorded");
        assert_eq!(cmp.local_status, Some(IntegrityStatus::Verified));
        assert!(r.detail.starts_with("local detail; "), "{}", r.detail);
    }

    #[test]
    fn apply_record_missing_note_with_record_stays_missing() {
        let mut v = version_of(NOTE_BODY, "2026-11-01T00:00:00Z");
        v.seeded = true;
        let record = NoteRecord { versions: vec![v] };
        let mut r = integrity(IntegrityStatus::Missing);
        let cmp = apply_record(&mut r, None, Some(&record), CompareMode::Content).unwrap();
        assert_eq!(r.status, IntegrityStatus::Missing);
        assert_eq!(cmp.record, "recorded");
        assert!(
            r.detail
                .contains("the record starts at the server's upgrade"),
            "{}",
            r.detail
        );
        let mut none = integrity(IntegrityStatus::Missing);
        assert_eq!(
            apply_record(&mut none, None, None, CompareMode::Content),
            None
        );
    }

    #[test]
    fn seeded_first_version_is_named_in_the_detail() {
        let original = sealed(NOTE_BODY);
        let mut first = version_of(&original, "2026-10-01T00:00:00Z");
        first.seeded = true;
        let record = NoteRecord {
            versions: vec![first],
        };
        let mut r = integrity(IntegrityStatus::Verified);
        apply_record(&mut r, Some("edited"), Some(&record), CompareMode::Content).unwrap();
        assert_eq!(r.status, IntegrityStatus::ChangedAfterRecorded);
        assert!(
            r.detail
                .contains("the record starts at the server's upgrade on 2026-10-01T00:00:00Z"),
            "{}",
            r.detail
        );
        assert!(r.detail.contains("1 version on record"), "{}", r.detail);
    }
}
