//! `autter verify` against the server's record of each note (tamper-evident
//! attribution, stage 2; src/authorship/note_server_record.rs).
//!
//! The integrity check alone cannot tell when someone who can write the
//! repository edits a note and recomputes its digest. In connected mode the
//! server keeps every version of each note it receives, and verify compares
//! with that record. These tests run verify against a small HTTP server that
//! serves `GET /worker/notes/record` as the backend does.
use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use autter::authorship::note_integrity::{read_integrity, seal_note};
use autter::authorship::note_server_record::note_content_sha256;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// A tiny notes server
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ServerState {
    /// Status and body for `GET /worker/notes/record`.
    record: (u16, String),
    /// Request lines received (method and path).
    requests: Vec<String>,
    /// Whether each record request carried the API key.
    record_had_api_key: Vec<bool>,
}

struct NotesServer {
    url: String,
    state: Arc<Mutex<ServerState>>,
}

impl NotesServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(ServerState {
            record: (200, r#"{"records":{}}"#.to_string()),
            ..Default::default()
        }));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut has_api_key = false;
                let mut content_length = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
                        break;
                    }
                    let lower = header.to_ascii_lowercase();
                    if lower.starts_with("x-api-key:") {
                        has_api_key = true;
                    }
                    if let Some(len) = lower.strip_prefix("content-length:") {
                        content_length = len.trim().parse().unwrap_or(0);
                    }
                    if header == "\r\n" || header == "\n" {
                        break;
                    }
                }
                if content_length > 0 {
                    let mut body = vec![0u8; content_length];
                    let _ = reader.read_exact(&mut body);
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let (status, body) = {
                    let mut st = shared.lock().unwrap();
                    st.requests.push(request_line.trim().to_string());
                    if path.starts_with("/worker/notes/record") {
                        st.record_had_api_key.push(has_api_key);
                        st.record.clone()
                    } else if path.starts_with("/worker/notes") {
                        (200, r#"{"notes":{}}"#.to_string())
                    } else {
                        (404, "{}".to_string())
                    }
                };
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        NotesServer { url, state }
    }

    fn serve_records(&self, records: Value) {
        self.state.lock().unwrap().record = (200, json!({ "records": records }).to_string());
    }

    fn serve_status(&self, status: u16) {
        self.state.lock().unwrap().record = (status, r#"{"error":"not found"}"#.to_string());
    }

    fn record_requests(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.contains("/worker/notes/record"))
            .cloned()
            .collect()
    }
}

/// One recorded version of `note`, as the server stores it.
fn version(note: &str, recorded_at: &str, uploaded_by: &str, seeded: bool) -> Value {
    json!({
        "content_sha256": note_content_sha256(note),
        "integrity_digest": read_integrity(note).and_then(|i| i.digest),
        "uploaded_by": uploaded_by,
        "recorded_at": recorded_at,
        "seeded": seeded,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn raw_note(repo: &TestRepo, commit: &str) -> String {
    repo.sync_daemon();
    repo.git_og(&["notes", "--ref=ai", "show", commit])
        .unwrap_or_else(|e| panic!("commit {commit} should have a note: {e}"))
}

/// Replace `commit`'s note with `content`, byte for byte.
fn write_raw_note(repo: &TestRepo, commit: &str, content: &str) {
    let git_dir = repo
        .git_og(&["rev-parse", "--absolute-git-dir"])
        .unwrap()
        .trim()
        .to_string();
    let path = std::path::Path::new(&git_dir).join("autter-test-note");
    fs::write(&path, content).unwrap();
    let blob = repo
        .git_og(&["hash-object", "-w", path.to_str().unwrap()])
        .unwrap();
    repo.git_og(&["notes", "--ref=ai", "add", "-f", "-C", blob.trim(), commit])
        .unwrap();
}

/// The attestation section's first range line with its ranges replaced.
fn replace_first_range(note: &str, ranges: &str) -> String {
    let divider = note.find("\n---\n").expect("note has a divider");
    let (attestations, rest) = note.split_at(divider);
    let mut done = false;
    let edited: Vec<String> = attestations
        .lines()
        .map(|line| {
            if !done && line.starts_with("  ") {
                done = true;
                let hash = line.split_whitespace().next().unwrap();
                format!("  {hash} {ranges}")
            } else {
                line.to_string()
            }
        })
        .collect();
    assert!(done, "note has no range line:\n{note}");
    format!("{}{}", edited.join("\n"), rest)
}

/// Run `autter verify <args>` in connected mode against `server` (or in the
/// test's default local mode when `server` is `None`): (exit code, stdout,
/// stderr).
fn run_verify(
    repo: &TestRepo,
    server: Option<&NotesServer>,
    args: &[&str],
) -> (i32, String, String) {
    repo.sync_daemon();
    let mut full = vec!["verify"];
    full.extend_from_slice(args);
    let mut command = repo.autter_command(&full);
    if let Some(server) = server {
        command
            .env("AUTTER_NOTES_BACKEND_KIND", "both")
            .env("AUTTER_NOTES_BACKEND_URL", &server.url)
            .env("AUTTER_API_KEY", "test-api-key");
    }
    let out = command.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// `autter verify <target> --json <extra>`: (exit code, document, stderr).
fn verify_json(
    repo: &TestRepo,
    server: Option<&NotesServer>,
    target: &str,
    extra: &[&str],
) -> (i32, Value, String) {
    let mut args = vec![target, "--json"];
    args.extend_from_slice(extra);
    let (code, stdout, stderr) = run_verify(repo, server, &args);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON from autter verify {target}\n{stdout}\n{stderr}"));
    (code, serde_json::from_str(line).unwrap(), stderr)
}

/// A repository with one commit whose note credits a line to AI.
fn repo_with_ai_commit() -> (TestRepo, String) {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["ai one".ai(), "human two"]);
    let commit = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);
    (repo, commit)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The note matches the only version the server recorded: verified, and the
/// result says the record matches.
#[test]
fn test_note_matching_the_record_verifies() {
    let (repo, commit) = repo_with_ai_commit();
    let note = raw_note(&repo, &commit);
    let server = NotesServer::start();
    server.serve_records(json!({
        commit.clone(): {"versions": [version(&note, "2026-11-01T10:00:00Z", "user_42", false)]}
    }));

    let (code, doc, stderr) = verify_json(&repo, Some(&server), &commit, &[]);
    assert_eq!(code, 0, "{doc}\n{stderr}");
    let result = &doc["results"][0];
    assert_eq!(result["status"], "verified", "{doc}");
    assert_eq!(result["record"], "matches", "{doc}");
    assert_eq!(result["record_summary"]["versions"], 1, "{doc}");
    assert_eq!(doc["record_check"]["checked"], true, "{doc}");
    assert_eq!(doc["record_check"]["mode"], "content", "{doc}");
    assert!(!stderr.contains("Server record not checked"), "{stderr}");

    let requests = server.record_requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].contains(&format!("/worker/notes/record?commits={commit}")),
        "{requests:?}"
    );
    assert!(server.state.lock().unwrap().record_had_api_key[0]);
}

/// THE case stage 2 exists for: someone edits the note's attestation and
/// recomputes the digest the way autter does. The integrity check alone
/// still says `verified`; the server's record shows the note changed.
#[test]
fn test_edited_note_with_recomputed_digest_fails_against_the_record() {
    let (repo, commit) = repo_with_ai_commit();
    let original = raw_note(&repo, &commit);
    let integrity = read_integrity(&original).unwrap();
    // Claim the human line was AI too, and reseal.
    let edited = seal_note(&replace_first_range(&original, "1-2"), &integrity).unwrap();
    write_raw_note(&repo, &commit, &edited);

    // Local-only: the edit is invisible to the integrity check.
    let (code, doc, stderr) = verify_json(&repo, None, &commit, &[]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["results"][0]["status"], "verified", "{doc}");
    assert_eq!(doc["record_check"]["checked"], false, "{doc}");
    assert!(
        doc["record_check"]["reason"]
            .as_str()
            .unwrap()
            .contains("local mode"),
        "{doc}"
    );
    assert!(
        stderr.contains("Server record not checked: local mode"),
        "{stderr}"
    );

    // Connected: the server recorded the original.
    let server = NotesServer::start();
    server.serve_records(json!({
        commit.clone(): {"versions": [version(&original, "2026-11-01T10:00:00Z", "user_42", false)]}
    }));
    let (code, doc, _) = verify_json(&repo, Some(&server), &commit, &[]);
    assert_eq!(code, 5, "{doc}");
    let result = &doc["results"][0];
    assert_eq!(result["status"], "changed_after_recorded", "{doc}");
    assert_eq!(result["local_status"], "verified", "{doc}");
    assert_eq!(result["record"], "changed", "{doc}");
    assert_eq!(doc["failed"], 1, "{doc}");
    assert_eq!(doc["counts"]["changed_after_recorded"], 1, "{doc}");
    assert_eq!(doc["error"]["code"], "integrity_failed", "{doc}");
    assert_eq!(doc["error"]["exit_code"], 5, "{doc}");

    // The human report names when and by whom the first version was recorded.
    let (code, stdout, stderr) = run_verify(&repo, Some(&server), &[&commit]);
    assert_eq!(code, 5, "{stdout}\n{stderr}");
    assert!(stdout.contains("FAIL"), "{stdout}");
    assert!(stdout.contains("changed_after_recorded"), "{stdout}");
    assert!(
        stdout.contains("first recorded 2026-11-01T10:00:00Z by user_42"),
        "{stdout}"
    );
    assert!(stdout.contains("1 version on record"), "{stdout}");
    assert!(stdout.contains("matches none of them"), "{stdout}");

    // Putting the original note back passes again.
    write_raw_note(&repo, &commit, &original);
    let (code, doc, _) = verify_json(&repo, Some(&server), &commit, &[]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["results"][0]["record"], "matches", "{doc}");
}

/// A record with more than one version fails even when the note here is the
/// first one; a seeded first version is reported as the start of the record.
#[test]
fn test_record_with_several_versions_fails() {
    let (repo, commit) = repo_with_ai_commit();
    let original = raw_note(&repo, &commit);
    let integrity = read_integrity(&original).unwrap();
    let edited = seal_note(&replace_first_range(&original, "1-2"), &integrity).unwrap();

    let server = NotesServer::start();
    server.serve_records(json!({
        commit.clone(): {"versions": [
            version(&original, "2026-10-01T00:00:00Z", "user_42", true),
            version(&edited, "2026-11-02T00:00:00Z", "user_7", false),
        ]}
    }));
    let (code, doc, _) = verify_json(&repo, Some(&server), &commit, &[]);
    assert_eq!(code, 5, "{doc}");
    let result = &doc["results"][0];
    assert_eq!(result["status"], "changed_after_recorded", "{doc}");
    assert_eq!(result["record_summary"]["versions"], 2, "{doc}");
    assert_eq!(result["record_summary"]["matches_version"], 1, "{doc}");
    assert_eq!(result["record_summary"]["first_seeded"], true, "{doc}");
    let detail = result["detail"].as_str().unwrap();
    assert!(detail.contains("2 versions on record"), "{detail}");
    assert!(
        detail.contains("the record starts at the server's upgrade on 2026-10-01T00:00:00Z"),
        "{detail}"
    );

    // The note here matching only the later version fails the same way.
    write_raw_note(&repo, &commit, &edited);
    let (code, doc, _) = verify_json(&repo, Some(&server), &commit, &[]);
    assert_eq!(code, 5, "{doc}");
    assert_eq!(
        doc["results"][0]["record_summary"]["matches_version"], 2,
        "{doc}"
    );
}

/// A note the server has no record of yet is a warning (uploads happen in
/// the background), also with --strict; --require-record fails it.
#[test]
fn test_not_recorded_warns_and_require_record_fails() {
    let (repo, commit) = repo_with_ai_commit();
    let server = NotesServer::start();
    server.serve_records(json!({}));

    let (code, doc, _) = verify_json(&repo, Some(&server), &commit, &[]);
    assert_eq!(code, 0, "{doc}");
    let result = &doc["results"][0];
    assert_eq!(result["status"], "not_recorded", "{doc}");
    assert_eq!(result["local_status"], "verified", "{doc}");
    assert_eq!(result["record"], "not_recorded", "{doc}");
    assert_eq!(doc["failed"], 0, "{doc}");

    let (code, doc, _) = verify_json(&repo, Some(&server), &commit, &["--strict"]);
    assert_eq!(code, 0, "{doc}");

    let (code, doc, _) = verify_json(&repo, Some(&server), &commit, &["--require-record"]);
    assert_eq!(code, 5, "{doc}");
    assert_eq!(doc["failed"], 1, "{doc}");
    assert_eq!(doc["record_check"]["required"], true, "{doc}");
    assert_eq!(doc["error"]["code"], "integrity_failed", "{doc}");
}

/// An older server without the record endpoint (404): the record is not
/// checked, verify says so and passes; --require-record fails.
#[test]
fn test_server_without_record_endpoint_is_not_a_failure() {
    let (repo, commit) = repo_with_ai_commit();
    let server = NotesServer::start();
    server.serve_status(404);

    let (code, doc, stderr) = verify_json(&repo, Some(&server), &commit, &[]);
    assert_eq!(code, 0, "{doc}\n{stderr}");
    assert_eq!(doc["results"][0]["status"], "verified", "{doc}");
    assert!(doc["results"][0].get("record").is_none(), "{doc}");
    assert_eq!(doc["record_check"]["checked"], false, "{doc}");
    assert!(
        doc["record_check"]["reason"]
            .as_str()
            .unwrap()
            .contains("does not keep a record"),
        "{doc}"
    );
    assert!(stderr.contains("Server record not checked"), "{stderr}");

    let (code, stdout, stderr) = run_verify(&repo, Some(&server), &[&commit]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(stdout.contains("ok   "), "{stdout}");
    assert!(
        stderr.contains("Server record not checked: the server does not keep a record"),
        "{stderr}"
    );

    let (code, doc, _) = verify_json(&repo, Some(&server), &commit, &["--require-record"]);
    assert_eq!(code, 5, "{doc}");
    assert_eq!(doc["failed"], 0, "{doc}");
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("--require-record"),
        "{doc}"
    );
}

/// A note deleted here while the server has a record of it stays `missing`
/// (a warning) and the result says the server recorded one.
#[test]
fn test_deleted_note_with_record_reports_the_record() {
    let (repo, commit) = repo_with_ai_commit();
    let note = raw_note(&repo, &commit);
    repo.git_og(&["notes", "--ref=ai", "remove", &commit])
        .unwrap();

    let server = NotesServer::start();
    server.serve_records(json!({
        commit.clone(): {"versions": [version(&note, "2026-11-01T10:00:00Z", "user_42", false)]}
    }));
    let (code, doc, _) = verify_json(&repo, Some(&server), &commit, &[]);
    assert_eq!(code, 0, "{doc}");
    let result = &doc["results"][0];
    assert_eq!(result["status"], "missing", "{doc}");
    assert_eq!(result["record"], "recorded", "{doc}");
    assert!(
        result["detail"]
            .as_str()
            .unwrap()
            .contains("the server recorded a note for this commit"),
        "{doc}"
    );
}

/// --no-record does not contact the server; it cannot be combined with
/// --require-record.
#[test]
fn test_no_record_skips_the_server() {
    let (repo, commit) = repo_with_ai_commit();
    let server = NotesServer::start();
    server.serve_records(json!({}));

    let (code, doc, stderr) = verify_json(&repo, Some(&server), &commit, &["--no-record"]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["results"][0]["status"], "verified", "{doc}");
    assert_eq!(doc["record_check"]["checked"], false, "{doc}");
    assert_eq!(doc["record_check"]["reason"], "--no-record", "{doc}");
    assert!(!stderr.contains("Server record not checked"), "{stderr}");
    assert!(server.record_requests().is_empty());

    let (code, stdout, stderr) = run_verify(
        &repo,
        Some(&server),
        &[&commit, "--no-record", "--require-record"],
    );
    assert_eq!(code, 2, "{stdout}\n{stderr}");
}

/// A range is fetched in one request per 100 commits and each commit is
/// compared with its own record.
#[test]
fn test_range_compares_each_commit() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["base"]);
    let base = repo.stage_all_and_commit("base").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["base".human()]);

    file.set_contents(crate::lines!["base", "ai one".ai()]);
    let first = repo.stage_all_and_commit("first").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["base".human(), "ai one".ai()]);

    file.set_contents(crate::lines!["base", "ai one", "ai two".ai()]);
    let second = repo.stage_all_and_commit("second").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["base".human(), "ai one".ai(), "ai two".ai()]);

    let first_note = raw_note(&repo, &first);
    let second_note = raw_note(&repo, &second);
    let integrity = read_integrity(&second_note).unwrap();
    let edited = seal_note(&replace_first_range(&second_note, "1-3"), &integrity).unwrap();
    write_raw_note(&repo, &second, &edited);

    let server = NotesServer::start();
    server.serve_records(json!({
        first.clone(): {"versions": [version(&first_note, "2026-11-01T10:00:00Z", "user_42", false)]},
        second.clone(): {"versions": [version(&second_note, "2026-11-01T11:00:00Z", "user_42", false)]},
    }));
    let range = format!("{base}..HEAD");
    let (code, doc, _) = verify_json(&repo, Some(&server), &range, &[]);
    assert_eq!(code, 5, "{doc}");
    assert_eq!(doc["commits"], 2, "{doc}");
    assert_eq!(doc["failed"], 1, "{doc}");
    let status_of = |sha: &str| {
        doc["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["commit"] == sha)
            .map(|r| r["status"].as_str().unwrap().to_string())
            .unwrap()
    };
    assert_eq!(status_of(&first), "verified");
    assert_eq!(status_of(&second), "changed_after_recorded");
    let requests = server.record_requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
}
