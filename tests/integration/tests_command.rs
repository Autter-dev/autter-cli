//! `autter tests` against a mock Autter API: it finds the PR from the branch,
//! writes the tests Autter kept into the checkout, runs them with a command it
//! builds itself (never the stored string), refuses database tests without
//! `--database-url`, never overwrites a file it did not write, and `clean`
//! removes exactly what it wrote.

use crate::repos::test_repo::{TestRepo, connected_mode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mockito::Matcher;
use serde_json::{Value, json};
use std::fs;
use std::process::{Output, Stdio};

const FAILING: &str = "src/sum.autter.test.mjs";
const PASSING: &str = "src/sum2.autter.test.mjs";
const DATABASE: &str = "src/store.autter.test.mjs";

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A signed-in account whose access token names the org `acme`.
fn sign_in(repo: &TestRepo) {
    let claims = json!({
        "sub": "user-1",
        "email": "dev@example.com",
        "org_id": "org-1",
        "orgs": [{ "org_id": "org-1", "org_slug": "acme", "org_name": "Acme" }],
    });
    let token = format!(
        "eyJhbGciOiJub25lIn0.{}.sig",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let path = repo
        .test_home_path()
        .join(".autter")
        .join("internal")
        .join("credentials");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let far_future = 4_102_444_800i64; // 2100-01-01
    fs::write(
        &path,
        json!({
            "access_token": token,
            "refresh_token": "test-refresh-token",
            "access_token_expires_at": far_future,
            "refresh_token_expires_at": far_future,
        })
        .to_string(),
    )
    .unwrap();
}

/// A checkout on `feat/sum` whose `sum` subtracts: the bug Autter's test caught.
fn fixture() -> (TestRepo, String) {
    let mut repo = TestRepo::new();
    repo.patch_autter_config(connected_mode);
    repo.git_og(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/calc.git",
    ])
    .unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("package.json"),
        "{\"name\":\"calc\",\"type\":\"module\"}\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("src/sum.js"),
        "export function sum(a, b) {\n  return a - b;\n}\n",
    )
    .unwrap();
    repo.stage_all_and_commit("sum").unwrap();
    repo.git_og(&["checkout", "-b", "feat/sum"]).unwrap();
    let head = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    sign_in(&repo);
    (repo, head)
}

fn test_source(expect: i32) -> String {
    format!(
        "import {{ test }} from 'node:test';\nimport assert from 'node:assert/strict';\nimport {{ sum }} from './sum.js';\ntest('adds', () => assert.equal(sum(1, 2), {expect}));\n"
    )
}

fn payload(head: &str) -> Value {
    let test = |path: &str, status: &str, source: String, db: bool| {
        json!({
            "gapId": format!("src/sum.js#{path}"),
            "path": path,
            "dir": "",
            "framework": "node-test",
            // Never executed: the CLI builds its own command.
            "command": "touch PWNED && node --test",
            "source": source,
            "cases": ["adds"],
            "status": status,
            "behaviour": "existing",
            "usesDatabase": db,
            "databaseEnvNames": if db { json!(["AUTTER_TEST_DATABASE_URL", "DATABASE_URL"]) } else { json!([]) },
            "envNames": [],
        })
    };
    json!({
        "repository": "acme/calc",
        "pullRequest": { "number": 12, "title": "Sum", "state": "open", "headRef": "feat/sum", "headSha": head, "htmlUrl": null },
        "generatedTests": {
            "headSha": head,
            "baseSha": "base",
            "isStale": false,
            "tests": [
                test(FAILING, "failed", test_source(3), false),
                test(PASSING, "passed", test_source(-1), false),
                test(DATABASE, "failed", "process.env.DATABASE_URL".into(), true),
            ],
            "failures": [{ "gapId": "src/sum.js#sum", "testPath": FAILING, "testName": "adds", "explanation": "sum subtracts instead of adding.", "location": "src/sum.js:2", "output": "" }],
            "couldNotRun": [],
            "dropped": [],
        },
    })
}

fn serve(server: &mut mockito::Server, head: &str) -> mockito::Mock {
    server
        .mock("GET", "/api/cli/orgs/acme/generated-tests")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("repository".into(), "https://github.com/acme/calc".into()),
            Matcher::UrlEncoded("branch".into(), "feat/sum".into()),
        ]))
        .match_header("authorization", Matcher::Regex("^Bearer ".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(payload(head).to_string())
        .create()
}

fn autter(repo: &TestRepo, server: &mockito::Server, args: &[&str]) -> Output {
    repo.autter_command(args)
        .env("AUTTER_API_BASE_URL", server.url())
        .env_remove("DATABASE_URL")
        .stdin(Stdio::null())
        .output()
        .expect("autter tests should run")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn tests_list_shows_what_autter_wrote_for_the_branchs_pr() {
    let (repo, head) = fixture();
    let mut server = mockito::Server::new();
    let mock = serve(&mut server, &head);
    let output = autter(&repo, &server, &["tests", "list"]);
    mock.assert();
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("acme/calc #12 — Sum (feat/sum)"), "{out}");
    assert!(out.contains("✗ src/sum.autter.test.mjs"), "{out}");
    assert!(out.contains("sum subtracts instead of adding."), "{out}");
    assert!(out.contains("(needs a database)"), "{out}");
    assert!(!repo.path().join(FAILING).exists(), "list writes nothing");
}

#[test]
fn tests_run_asks_first_and_refuses_without_a_terminal() {
    let (repo, head) = fixture();
    let mut server = mockito::Server::new();
    serve(&mut server, &head);
    let output = autter(&repo, &server, &["tests", "run"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("Pass --yes"), "{out}");
    assert!(
        !repo.path().join(FAILING).exists(),
        "nothing written before the answer"
    );
}

#[test]
fn tests_run_writes_and_runs_the_failing_tests_then_clean_removes_them() {
    if !node_available() {
        eprintln!("skipping: node is not installed");
        return;
    }
    let (repo, head) = fixture();
    let mut server = mockito::Server::new();
    serve(&mut server, &head);

    let output = autter(&repo, &server, &["tests", "run", "--yes"]);
    let out = text(&output);
    // The failing test reproduces the bug, and the database test is skipped: exit 1.
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("node --test src/sum.autter.test.mjs"), "{out}");
    assert!(out.contains("✗ src/sum.autter.test.mjs failed"), "{out}");
    assert!(out.contains("src/store.autter.test.mjs skipped"), "{out}");
    assert!(out.contains("--database-url"), "{out}");
    assert!(repo.path().join(FAILING).exists());
    assert!(
        !repo.path().join(DATABASE).exists(),
        "a skipped test is not written"
    );
    assert!(
        !repo.path().join(PASSING).exists(),
        "passing tests only with --all"
    );
    assert!(
        !repo.path().join("PWNED").exists(),
        "the stored command never runs"
    );

    // Fix the bug: the same test passes.
    fs::write(
        repo.path().join("src/sum.js"),
        "export function sum(a, b) {\n  return a + b;\n}\n",
    )
    .unwrap();
    let output = autter(
        &repo,
        &server,
        &["tests", "run", "--yes", "--only", FAILING, "--json"],
    );
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    let doc: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(doc["results"][0]["run"]["result"], "passed", "{out}");

    let output = autter(&repo, &server, &["tests", "clean"]);
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("removed src/sum.autter.test.mjs"), "{out}");
    assert!(!repo.path().join(FAILING).exists());
    assert!(
        repo.path().join("src/sum.js").exists(),
        "clean removes only what pull wrote"
    );
}

#[test]
fn tests_pull_never_overwrites_a_file_autter_did_not_write() {
    let (repo, head) = fixture();
    let mut server = mockito::Server::new();
    serve(&mut server, &head);
    fs::write(repo.path().join(FAILING), "// my own file\n").unwrap();
    let output = autter(&repo, &server, &["tests", "pull", "--all", "--force"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(
        out.contains("a file Autter did not write is already there"),
        "{out}"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join(FAILING)).unwrap(),
        "// my own file\n"
    );
    assert!(
        repo.path().join(PASSING).exists(),
        "the other files are still written"
    );

    let output = autter(&repo, &server, &["tests", "clean"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(!repo.path().join(PASSING).exists());
    assert!(repo.path().join(FAILING).exists(), "the user's file stays");
}

#[test]
fn tests_run_with_a_database_url_sets_the_recorded_names() {
    if !node_available() {
        eprintln!("skipping: node is not installed");
        return;
    }
    let (repo, head) = fixture();
    let mut server = mockito::Server::new();
    // The database test prints and checks the URL it was given.
    let mut body = payload(&head);
    body["generatedTests"]["tests"][2]["source"] = json!(
        "import { test } from 'node:test';\nimport assert from 'node:assert/strict';\ntest('db', () => { assert.equal(process.env.DATABASE_URL, 'postgres://u:p@localhost:5432/scratch'); assert.equal(process.env.AUTTER_TEST_DATABASE_URL, process.env.DATABASE_URL); assert.equal(process.env.CI, 'true'); });\n"
    );
    server
        .mock("GET", "/api/cli/orgs/acme/generated-tests")
        .match_query(Matcher::UrlEncoded("pr".into(), "12".into()))
        .with_status(200)
        .with_body(body.to_string())
        .create();
    let output = autter(
        &repo,
        &server,
        &[
            "tests",
            "run",
            "--pr",
            "12",
            "--yes",
            "--only",
            DATABASE,
            "--database-url",
            "postgres://u:p@localhost:5432/scratch",
        ],
    );
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("✓ src/store.autter.test.mjs passed"), "{out}");
}

#[test]
fn tests_reports_the_servers_message_when_there_are_no_tests() {
    let (repo, _) = fixture();
    let mut server = mockito::Server::new();
    server
        .mock("GET", "/api/cli/orgs/acme/generated-tests")
        .match_query(Matcher::Any)
        .with_status(404)
        .with_body(json!({ "error": "pr_not_found", "message": "Autter has no pull request from `feat/sum` in acme/calc." }).to_string())
        .create();
    let output = autter(&repo, &server, &["tests", "list"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(
        out.contains("Autter has no pull request from `feat/sum`"),
        "{out}"
    );
}
