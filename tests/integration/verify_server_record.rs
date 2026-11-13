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
    /// Every note received by `POST /worker/notes/upload`, in order.
    uploads: Vec<(String, String)>,
    /// Serve `GET /worker/notes/record` from `uploads`, as the backend
    /// does, instead of from `record`.
    record_from_uploads: bool,
}

impl ServerState {
    /// The record the backend would keep from `uploads`: every distinct
    /// version of each commit's note, oldest first.
    fn record_from_uploads(&self) -> String {
        let mut records = serde_json::Map::new();
        for (sha, content) in &self.uploads {
            let entry = records
                .entry(sha.clone())
                .or_insert_with(|| json!({"versions": []}));
            let versions = entry["versions"].as_array_mut().unwrap();
            let v = version(content, "2026-11-01T10:00:00Z", "user_42", false);
            if !versions
                .iter()
                .any(|x| x["content_sha256"] == v["content_sha256"])
            {
                versions.push(v);
            }
        }
        json!({ "records": records }).to_string()
    }
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
                let mut body = vec![0u8; content_length];
                if content_length > 0 {
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
                        if st.record_from_uploads {
                            (200, st.record_from_uploads())
                        } else {
                            st.record.clone()
                        }
                    } else if path.starts_with("/worker/notes/upload") {
                        let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        let entries = request["entries"].as_array().cloned().unwrap_or_default();
                        for e in &entries {
                            st.uploads.push((
                                e["commit_sha"].as_str().unwrap_or_default().to_string(),
                                e["content"].as_str().unwrap_or_default().to_string(),
                            ));
                        }
                        (
                            200,
                            json!({"success_count": entries.len(), "failure_count": 0}).to_string(),
                        )
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

    /// Act as the backend: keep what is uploaded and serve it as the record.
    fn record_uploads(&self) {
        self.state.lock().unwrap().record_from_uploads = true;
    }

    /// The distinct note contents uploaded for `commit`.
    fn uploaded_versions(&self, commit: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (sha, content) in &self.state.lock().unwrap().uploads {
            if sha == commit && !out.iter().any(|c| c.trim_end() == content.trim_end()) {
                out.push(content.clone());
            }
        }
        out
    }

    /// Wait until every commit in `commits` has an upload (the background
    /// service uploads after each note write and every few seconds).
    fn wait_for_uploads(&self, commits: &[String]) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let done = {
                let st = self.state.lock().unwrap();
                commits
                    .iter()
                    .all(|c| st.uploads.iter().any(|(sha, _)| sha == c))
            };
            if done {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no upload for {commits:?}; requests: {:?}",
                self.state.lock().unwrap().requests
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
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
    // Local-only mode has no record: no stderr line on every run.
    assert!(!stderr.contains("Server record not checked"), "{stderr}");
    let (code, stdout, stderr) = run_verify(&repo, None, &[&commit]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(!stderr.contains("Server record not checked"), "{stderr}");
    // ...unless the record was asked for.
    let (code, stdout, stderr) = run_verify(&repo, None, &[&commit, "--require-record"]);
    assert_eq!(code, 5, "{stdout}\n{stderr}");
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

// ---------------------------------------------------------------------------
// Commits that keep their SHA keep their note
//
// Every note write is uploaded in connected mode, and the server keeps every
// version. A rewrite that leaves a commit's SHA as it was must not write a
// second, different note for it, or `autter verify` would report
// `changed_after_recorded` for a commit nobody touched. Each test records the
// notes, runs the operation, checks the notes are byte for byte the same,
// and checks `autter verify` against a record of the original notes.
// ---------------------------------------------------------------------------

/// The notes of `commits`, in order.
fn notes_of(repo: &TestRepo, commits: &[String]) -> Vec<(String, String)> {
    commits
        .iter()
        .map(|c| (c.clone(), raw_note(repo, c)))
        .collect()
}

/// Each commit still has exactly the note it had, and `autter verify`
/// matches a server record holding only those notes.
fn assert_notes_kept(repo: &TestRepo, before: &[(String, String)], status: &str) {
    for (commit, note) in before {
        assert_eq!(
            &raw_note(repo, commit),
            note,
            "the note of {commit} was rewritten although its SHA did not change"
        );
    }
    let server = NotesServer::start();
    let mut records = serde_json::Map::new();
    for (commit, note) in before {
        records.insert(
            commit.clone(),
            json!({"versions": [version(note, "2026-11-01T10:00:00Z", "user_42", false)]}),
        );
    }
    server.serve_records(Value::Object(records));
    for (commit, _) in before {
        let (code, doc, _) = verify_json(repo, Some(&server), commit, &[]);
        assert_eq!(code, 0, "{doc}");
        assert_eq!(doc["results"][0]["status"], status, "{doc}");
        assert_eq!(doc["results"][0]["record"], "matches", "{doc}");
    }
}

fn rev(repo: &TestRepo, r: &str) -> String {
    repo.git_og(&["rev-parse", r]).unwrap().trim().to_string()
}

/// `main` with `base.txt`, and `feature` with two AI commits on top of it
/// (main does not move). Leaves `feature` checked out. Returns (main branch,
/// [first, second]).
fn feature_on_top_of_main(repo: &TestRepo) -> (String, Vec<String>) {
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut one = repo.filename("one.txt");
    one.set_contents(crate::lines!["human one", "ai one".ai()]);
    let first = repo.stage_all_and_commit("feature one").unwrap().commit_sha;
    one.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);

    let mut two = repo.filename("two.txt");
    two.set_contents(crate::lines!["ai two".ai(), "human two"]);
    let second = repo.stage_all_and_commit("feature two").unwrap().commit_sha;
    two.assert_committed_lines(crate::lines!["ai two".ai(), "human two".human()]);
    (main, vec![first, second])
}

fn assert_feature_lines(repo: &TestRepo) {
    let mut one = repo.filename("one.txt");
    one.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);
    let mut two = repo.filename("two.txt");
    two.assert_committed_lines(crate::lines!["ai two".ai(), "human two".human()]);
}

/// Case 1: `git rebase <main>` when the branch is already on main, and
/// `git rebase -i` whose picks all fast-forward: no commit changes SHA.
#[cfg(unix)]
#[test]
fn test_noop_rebases_keep_notes() {
    let repo = TestRepo::new();
    let (main, commits) = feature_on_top_of_main(&repo);
    let before = notes_of(&repo, &commits);

    repo.git(&["rebase", &main]).unwrap();
    assert_eq!(rev(&repo, "HEAD"), commits[1]);
    assert_feature_lines(&repo);

    repo.git_with_env(
        &["rebase", "-i", &main],
        &[("GIT_SEQUENCE_EDITOR", "true"), ("GIT_EDITOR", "true")],
        None,
    )
    .unwrap();
    assert_eq!(rev(&repo, "HEAD"), commits[1]);
    assert_feature_lines(&repo);

    repo.git(&["rebase", "--onto", &main, &main]).unwrap();
    assert_eq!(rev(&repo, "HEAD"), commits[1]);
    assert_feature_lines(&repo);

    assert_notes_kept(&repo, &before, "verified");
}

/// Case 2: post-commit processing running a second time for a commit that
/// already has its note (the git proxy's fallback write followed by the
/// background service, or a retry). The second pass sees no pending
/// checkpoints and would write a different note.
#[test]
fn test_post_commit_twice_keeps_the_note() {
    let (repo, commit) = repo_with_ai_commit();
    let before = notes_of(&repo, std::slice::from_ref(&commit));

    let autter_repo =
        autter::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    autter::authorship::post_commit::post_commit(
        &autter_repo,
        None, // the root commit
        commit.clone(),
        "Test User <test@example.com>".to_string(),
        true,
    )
    .unwrap();

    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);
    assert_notes_kept(&repo, &before, "verified");
}

/// Case 3: `autter ci` run twice on the same squash merge (a CI re-run). The
/// second run finds the note and writes nothing; a run that does not find it
/// (a fresh clone the first run's notes did not reach) writes the same note
/// again, byte for byte, so the server sees one version.
#[test]
fn test_ci_rerun_on_the_same_merge_writes_one_note() {
    use autter::ci::ci_context::{CiContext, CiEvent, CiRunOptions};

    let repo = TestRepo::new();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    let base_sha = repo.stage_all_and_commit("base").unwrap().commit_sha;
    base.assert_committed_lines(crate::lines!["base".human()]);
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = repo.filename("feature.txt");
    feature.set_contents(crate::lines!["ai feature".ai(), "human feature"]);
    let head_sha = repo.stage_all_and_commit("feature").unwrap().commit_sha;
    feature.assert_committed_lines(crate::lines!["ai feature".ai(), "human feature".human()]);

    // A squash merge as the forge makes it: a new commit on main, made
    // without autter.
    repo.git_og(&["checkout", "main"]).unwrap();
    repo.git_og(&["merge", "--squash", "feature"]).unwrap();
    repo.git_og(&["commit", "-m", "Squash feature"]).unwrap();
    let merge_sha = rev(&repo, "HEAD");

    let run_ci = || {
        let ctx = CiContext::with_repository(
            autter::git::repository::find_repository_in_path(repo.path().to_str().unwrap())
                .unwrap(),
            CiEvent::Merge {
                merge_commit_sha: merge_sha.clone(),
                head_ref: "feature".to_string(),
                head_sha: head_sha.clone(),
                base_ref: "main".to_string(),
                base_sha: base_sha.clone(),
                fork_clone_url: None,
            },
        );
        ctx.run_with_options(CiRunOptions {
            skip_fetch_notes: true,
            skip_fetch_base: true,
            skip_fetch_fork_notes: true,
            skip_fetch_sync_refs: true,
            skip_push: true,
        })
        .unwrap();
    };

    run_ci();
    feature.assert_committed_lines(crate::lines!["ai feature".ai(), "human feature".human()]);
    let first = raw_note(&repo, &merge_sha);

    // Re-run: the note is there, nothing is written.
    run_ci();
    feature.assert_committed_lines(crate::lines!["ai feature".ai(), "human feature".human()]);
    assert_eq!(raw_note(&repo, &merge_sha), first);

    // Re-run where the first run's note is absent: the same note again.
    repo.git_og(&["notes", "--ref=ai", "remove", &merge_sha])
        .unwrap();
    run_ci();
    feature.assert_committed_lines(crate::lines!["ai feature".ai(), "human feature".human()]);
    assert_eq!(
        raw_note(&repo, &merge_sha),
        first,
        "a CI re-run must write the same note for the same merge commit"
    );

    assert_notes_kept(&repo, &[(merge_sha.clone(), first)], "verified");
}

/// Case 4: notes written before the integrity check (no `integrity`
/// member) on commits a rewrite leaves at the same SHA are kept as they are,
/// not stamped again.
#[cfg(unix)]
#[test]
fn test_legacy_notes_on_unchanged_commits_are_not_restamped() {
    let repo = TestRepo::new();
    let (main, commits) = feature_on_top_of_main(&repo);
    for commit in &commits {
        let legacy = autter::authorship::note_integrity::strip_integrity(&raw_note(&repo, commit));
        write_raw_note(&repo, commit, &legacy);
    }
    let before = notes_of(&repo, &commits);
    assert!(read_integrity(&before[0].1).is_none());

    repo.git(&["rebase", &main]).unwrap();
    repo.git_with_env(
        &["rebase", "-i", &main],
        &[("GIT_SEQUENCE_EDITOR", "true"), ("GIT_EDITOR", "true")],
        None,
    )
    .unwrap();
    assert_eq!(rev(&repo, "HEAD"), commits[1]);
    assert_feature_lines(&repo);

    // A cherry-pick that fast-forwards onto the same commit.
    repo.git(&["reset", "--hard", &commits[0]]).unwrap();
    repo.git(&["cherry-pick", "--ff", &commits[1]]).unwrap();
    assert_eq!(rev(&repo, "HEAD"), commits[1]);
    assert_feature_lines(&repo);

    assert_notes_kept(&repo, &before, "legacy");
}

/// Case 5: other rewrites whose new SHA equals the old one: a cherry-pick
/// that fast-forwards to the commit it picks, and an amend that changes
/// nothing (same tree, message and dates).
#[test]
fn test_same_sha_cherry_pick_and_amend_keep_notes() {
    let repo = TestRepo::new();
    let (_main, commits) = feature_on_top_of_main(&repo);
    let before = notes_of(&repo, &commits);

    repo.git(&["reset", "--hard", &commits[0]]).unwrap();
    repo.git(&["cherry-pick", "--ff", &commits[1]]).unwrap();
    repo.sync_daemon();
    assert_eq!(rev(&repo, "HEAD"), commits[1]);
    assert_feature_lines(&repo);

    let date = repo
        .git_og(&["log", "-1", "--format=%cI", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    repo.git_with_env(
        &["commit", "--amend", "--no-edit", "--allow-empty"],
        &[("GIT_COMMITTER_DATE", date.as_str())],
        None,
    )
    .unwrap();
    repo.sync_daemon();
    assert_eq!(
        rev(&repo, "HEAD"),
        commits[1],
        "an amend that changes nothing keeps the SHA"
    );
    assert_feature_lines(&repo);

    assert_notes_kept(&repo, &before, "verified");
}

/// Case 6: the writer itself, in local mode (`git_notes`, nothing is
/// uploaded). The same note again is not written; a different note for a
/// commit a rewrite keeps at its SHA is not written; a note git copied here
/// from another commit is replaced; and a corrected note for a commit that did
/// not keep its SHA is written, since no server has the note it replaces. In
/// connected mode that correction is written only while the note it replaces
/// has not been sent (`writer_keeps_a_note_that_may_be_on_the_server` in
/// src/git/notes_api.rs, and the connected tests below).
#[test]
fn test_writer_rules_for_a_commit_that_has_a_note() {
    use autter::authorship::note_integrity::strip_integrity;
    use autter::git::notes_api::{keep_existing_notes_for, write_note, write_notes_batch};

    let (repo, commit) = repo_with_ai_commit();
    let before = notes_of(&repo, std::slice::from_ref(&commit));
    let autter_repo =
        autter::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // The same note (as written before stamping): nothing to write.
    write_note(&autter_repo, &commit, &strip_integrity(&before[0].1)).unwrap();
    // A different note while a rewrite keeps the commit at its SHA.
    let other = replace_first_range(&strip_integrity(&before[0].1), "1-2");
    {
        let _keep = keep_existing_notes_for([commit.clone()]);
        write_note(&autter_repo, &commit, &other).unwrap();
        write_notes_batch(&autter_repo, &[(commit.clone(), other.clone())]).unwrap();
    }
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);
    assert_notes_kept(&repo, &before, "verified");

    // A note that names another commit (git copied it here) is replaced.
    let mut next = repo.filename("next.txt");
    next.set_contents(crate::lines!["ai next".ai()]);
    let second = repo.stage_all_and_commit("next").unwrap().commit_sha;
    next.assert_committed_lines(crate::lines!["ai next".ai()]);
    let second_note = raw_note(&repo, &second);
    repo.git_og(&["notes", "--ref=ai", "copy", "-f", &commit, &second])
        .unwrap();
    {
        let _keep = keep_existing_notes_for([second.clone()]);
        write_note(&autter_repo, &second, &strip_integrity(&second_note)).unwrap();
    }
    assert_eq!(raw_note(&repo, &second), second_note);
    next.assert_committed_lines(crate::lines!["ai next".ai()]);

    // A correction for a commit no rewrite keeps (a cherry-pick finished
    // with a plain commit) is written: in local mode no server has the note.
    write_note(&autter_repo, &commit, &other).unwrap();
    assert_eq!(
        strip_integrity(&raw_note(&repo, &commit)),
        strip_integrity(&other),
        "a corrected note for a new commit must be written"
    );
    // The edited note also keeps line 2's human entry, which blame prefers.
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);
}

/// Notes are now serialized with their entries in a fixed order (files by
/// path, entries by first line), so the same note computed twice is the same
/// text. A note written by autter 3.8.x has its entries in whatever order a
/// hash map gave. Its digest covers its own text, which is never
/// re-serialized when it is read or checked, so it still verifies, still
/// matches a server record of it, and a rewrite that keeps its commit's SHA
/// leaves it byte for byte as it is.
#[cfg(unix)]
#[test]
fn test_note_from_3_8_with_entries_in_another_order_still_verifies() {
    let repo = TestRepo::new();
    let (main, commits) = feature_on_top_of_main(&repo);
    let commit = &commits[1];

    // Rewrite the note as 3.8.x could have: the file's two entries swapped,
    // stamped the way 3.8.x stamps (seal_note is unchanged since 3.8.0).
    let note = raw_note(&repo, commit);
    let lines: Vec<&str> = note.lines().collect();
    assert!(
        lines[1].starts_with("  ") && lines[2].starts_with("  "),
        "expected two entries for two.txt:\n{note}"
    );
    let mut swapped: Vec<&str> = lines.clone();
    swapped.swap(1, 2);
    let body = autter::authorship::note_integrity::strip_integrity(&swapped.join("\n"));
    let old_style = seal_note(&body, &read_integrity(&note).unwrap()).unwrap();
    assert_ne!(old_style, note);
    write_raw_note(&repo, commit, &old_style);
    assert_feature_lines(&repo);

    // It verifies and matches a record of itself.
    let before = notes_of(&repo, std::slice::from_ref(commit));
    assert_notes_kept(&repo, &before, "verified");

    // A rewrite that keeps the SHA leaves it as it is.
    repo.git(&["rebase", &main]).unwrap();
    repo.git_with_env(
        &["rebase", "-i", &main],
        &[("GIT_SEQUENCE_EDITOR", "true"), ("GIT_EDITOR", "true")],
        None,
    )
    .unwrap();
    assert_eq!(rev(&repo, "HEAD"), *commit);
    assert_feature_lines(&repo);
    assert_notes_kept(&repo, &before, "verified");
}

// ---------------------------------------------------------------------------
// End to end in connected mode: the background service uploads to the mock
// server, which keeps every version like the backend.
// ---------------------------------------------------------------------------

/// A repository in connected mode whose background service syncs notes to
/// `server`.
fn connected_repo(server: &NotesServer) -> TestRepo {
    let mut repo = TestRepo::new_with_daemon_env(&[
        ("AUTTER_NOTES_BACKEND_KIND", "both"),
        ("AUTTER_NOTES_BACKEND_URL", server.url.as_str()),
        ("AUTTER_API_KEY", "test-api-key"),
    ]);
    let url = server.url.clone();
    repo.patch_autter_config(move |patch| {
        patch.prompt_storage = Some("default".to_string());
        patch.notes_backend = Some(autter::config::NotesBackendConfig {
            kind: autter::config::NotesBackendKind::Both,
            backend_url: Some(url),
        });
    });
    // Notes are routed by the repository they belong to; one without a remote
    // is never uploaded.
    repo.git_og(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/widgets.git",
    ])
    .unwrap();
    server.record_uploads();
    repo
}

/// Each commit's uploads hold one note, the one it has now, and `autter
/// verify` matches the record built from the uploads.
fn assert_one_version_uploaded(repo: &TestRepo, server: &NotesServer, commits: &[String]) {
    server.wait_for_uploads(commits);
    for commit in commits {
        let versions = server.uploaded_versions(commit);
        assert_eq!(
            versions.len(),
            1,
            "{commit} was uploaded with {} different notes: {versions:#?}",
            versions.len()
        );
        assert_eq!(versions[0].trim_end(), raw_note(repo, commit).trim_end());
        let (code, doc, _) = verify_json(repo, Some(server), commit, &[]);
        assert_eq!(code, 0, "{doc}");
        assert_eq!(doc["results"][0]["record"], "matches", "{doc}");
    }
}

/// Case 1 end to end: a no-op rebase in connected mode uploads nothing new.
#[cfg(unix)]
#[test]
fn test_connected_noop_rebase_uploads_one_version() {
    let server = NotesServer::start();
    let repo = connected_repo(&server);
    let (main, commits) = feature_on_top_of_main(&repo);
    server.wait_for_uploads(&commits);

    repo.git(&["rebase", &main]).unwrap();
    repo.git_with_env(
        &["rebase", "-i", &main],
        &[("GIT_SEQUENCE_EDITOR", "true"), ("GIT_EDITOR", "true")],
        None,
    )
    .unwrap();
    assert_eq!(rev(&repo, "HEAD"), commits[1]);
    assert_feature_lines(&repo);
    repo.sync_daemon();

    assert_one_version_uploaded(&repo, &server, &commits);
}

/// A cherry-pick stopped on a conflict and finished with a plain `git
/// commit` gets a post-commit note and then the cherry-pick's note for the
/// same new commit. Only the final note is uploaded.
#[test]
fn test_connected_cherry_pick_finished_with_plain_commit_uploads_one_version() {
    let server = NotesServer::start();
    let repo = connected_repo(&server);
    let file_path = repo.path().join("plain.txt");

    fs::write(&file_path, "Line 1\nLine 2\nLine 3\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("plain.txt");
    file.assert_committed_lines(crate::lines![
        "Line 1".unattributed_human(),
        "Line 2".unattributed_human(),
        "Line 3".unattributed_human(),
    ]);
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    repo.autter(&["checkpoint", "human", "plain.txt"]).unwrap();
    fs::write(&file_path, "Line 1\nAI version\nLine 3\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "plain.txt"])
        .unwrap();
    let source = repo.stage_all_and_commit("AI change").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines![
        "Line 1".unattributed_human(),
        "AI version".ai(),
        "Line 3".unattributed_human(),
    ]);

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(&file_path, "Line 1\nMain version\nLine 3\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "plain.txt"])
        .unwrap();
    repo.stage_all_and_commit("Human change").unwrap();
    file.assert_committed_lines(crate::lines![
        "Line 1".unattributed_human(),
        "Main version".human(),
        "Line 3".unattributed_human(),
    ]);

    assert!(repo.git(&["cherry-pick", &source]).is_err());
    repo.sync_daemon();
    fs::write(&file_path, "Line 1\nAI version\nLine 3\n").unwrap();
    repo.git(&["add", "plain.txt"]).unwrap();
    let picked = repo
        .commit("Finish pick with plain commit")
        .unwrap()
        .commit_sha;
    repo.sync_daemon();
    file.assert_committed_lines(crate::lines![
        "Line 1".unattributed_human(),
        "AI version".ai(),
        "Line 3".unattributed_human(),
    ]);

    assert_one_version_uploaded(&repo, &server, std::slice::from_ref(&picked));
}

/// Case 3 in connected mode: `autter ci` re-run on a squash merge whose note
/// was already uploaded, after the source commit's note changed (its notes
/// reached the clone late, or a newer autter computes it differently), so a
/// fresh computation would give a different note for the same merge commit.
/// `autter ci` finds the merge commit's note and writes nothing (the writer
/// would keep that note too: `writer_keeps_a_note_that_may_be_on_the_server`
/// in src/git/notes_api.rs); the record holds one version and `autter verify`
/// matches it.
#[test]
fn test_connected_ci_rerun_keeps_the_uploaded_note() {
    let server = NotesServer::start();
    let repo = connected_repo(&server);
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    let base_sha = repo.stage_all_and_commit("base").unwrap().commit_sha;
    base.assert_committed_lines(crate::lines!["base".human()]);
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = repo.filename("feature.txt");
    feature.set_contents(crate::lines!["ai feature".ai(), "human feature"]);
    let head_sha = repo.stage_all_and_commit("feature").unwrap().commit_sha;
    feature.assert_committed_lines(crate::lines!["ai feature".ai(), "human feature".human()]);

    // A squash merge as the forge makes it: a new commit on main with the
    // feature's tree (main did not move), made with plumbing so that nothing
    // on this machine attributes it before `autter ci` does.
    let merge_sha = repo
        .git_og(&[
            "commit-tree",
            "feature^{tree}",
            "-p",
            "main",
            "-m",
            "Squash feature",
        ])
        .unwrap()
        .trim()
        .to_string();
    repo.git_og(&["update-ref", "refs/heads/main", &merge_sha])
        .unwrap();
    repo.sync_daemon();
    assert!(
        repo.git_og(&["notes", "--ref=ai", "show", &merge_sha])
            .is_err(),
        "the squash commit has no note before CI runs"
    );

    let run_ci = || {
        repo.autter(&[
            "ci",
            "local",
            "merge",
            "--merge-commit-sha",
            &merge_sha,
            "--base-ref",
            "main",
            "--head-ref",
            "feature",
            "--head-sha",
            &head_sha,
            "--base-sha",
            &base_sha,
            "--skip-fetch",
            "--skip-push",
        ])
        .unwrap_or_else(|e| panic!("ci local merge failed:\n{e}"))
    };

    let output = run_ci();
    assert!(output.contains("Rewrote authorship"), "{output}");
    feature.assert_committed_lines(crate::lines!["ai feature".ai(), "human feature".human()]);
    let first = raw_note(&repo, &merge_sha);
    assert!(
        first.contains("feature.txt"),
        "the squash commit's note carries the feature's attribution:\n{first}"
    );
    server.wait_for_uploads(std::slice::from_ref(&merge_sha));
    assert_eq!(
        server.uploaded_versions(&merge_sha)[0].trim_end(),
        first.trim_end()
    );

    // The source commit's note changes (still a valid note: its digest is
    // recomputed), so the squash's attribution would compute differently.
    let source = raw_note(&repo, &head_sha);
    let changed = seal_note(
        &replace_first_range(
            &autter::authorship::note_integrity::strip_integrity(&source),
            "1-2",
        ),
        &read_integrity(&source).unwrap(),
    )
    .unwrap();
    write_raw_note(&repo, &head_sha, &changed);

    let output = run_ci();
    assert!(output.contains("already has authorship"), "{output}");
    feature.assert_committed_lines(crate::lines!["ai feature".ai(), "human feature".human()]);
    assert_eq!(
        raw_note(&repo, &merge_sha),
        first,
        "a CI re-run must keep the merge commit's uploaded note"
    );
    assert_one_version_uploaded(&repo, &server, std::slice::from_ref(&merge_sha));
}
