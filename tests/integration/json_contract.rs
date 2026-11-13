//! The scriptable-CLI contract: every reporting command accepts `--json`,
//! prints exactly one JSON document on stdout that validates against
//! `docs/schemas/autter-cli.schema.json`, carries `schema_version`, and exits
//! with the documented codes (0 success, 1 runtime error, 2 usage error,
//! 3 nothing captured, meaning autter has no record of the added lines, and
//! 4 capture broken, the last two only with `--strict`, 5 a note failed
//! `autter verify`). The shape of each document is snapshotted so a schema
//! change is always deliberate.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use serde_json::Value;
use std::fs;
use std::process::Output;

const SCHEMA_VERSION: u64 = 1;

/// The schema file, parsed once per test.
fn schema() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/schemas/autter-cli.schema.json");
    serde_json::from_str(&fs::read_to_string(&path).expect("read schema")).expect("schema is JSON")
}

pub(crate) struct Run {
    pub(crate) code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) fn run_output(output: Output) -> Run {
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Run autter in the test repo, after the daemon has written every note.
fn run(repo: &TestRepo, args: &[&str], envs: &[(&str, &str)]) -> Run {
    // Same rule as `TestRepo::autter`: only commands that read notes wait for
    // the daemon to finish writing them.
    let primary = args.iter().find(|arg| !arg.starts_with('-')).copied();
    if matches!(primary, Some("stats" | "blame" | "diff" | "log" | "show")) {
        repo.sync_daemon_force();
    }
    let mut command = repo.autter_command(args);
    command.env_remove("AUTTER_STRICT");
    for (key, value) in envs {
        command.env(key, value);
    }
    run_output(command.output().expect("run autter"))
}

/// Stdout must be one JSON document and nothing else.
pub(crate) fn parse_document(run: &Run, what: &str) -> Value {
    serde_json::from_str(run.stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "{what}: stdout is not exactly one JSON document ({e})\nexit: {}\nstdout:\n{}\nstderr:\n{}",
            run.code, run.stdout, run.stderr
        )
    })
}

// ---------------------------------------------------------------------------
// A small JSON Schema validator for the keywords the contract schema uses:
// type, const, enum, required, properties, additionalProperties, items, $ref
// (local), allOf and anyOf.
// ---------------------------------------------------------------------------

fn resolve<'a>(root: &'a Value, reference: &str) -> &'a Value {
    let pointer = reference
        .strip_prefix('#')
        .unwrap_or_else(|| panic!("only local $refs are supported: {reference}"));
    root.pointer(pointer)
        .unwrap_or_else(|| panic!("unresolved $ref {reference}"))
}

fn type_matches(value: &Value, ty: &str) -> bool {
    match ty {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        other => panic!("unsupported type {other}"),
    }
}

fn validate(root: &Value, schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        validate(root, resolve(root, reference), value, path, errors);
    }
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        for sub in all {
            validate(root, sub, value, path, errors);
        }
    }
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array) {
        let ok = any.iter().any(|sub| {
            let mut sub_errors = Vec::new();
            validate(root, sub, value, path, &mut sub_errors);
            sub_errors.is_empty()
        });
        if !ok {
            errors.push(format!("{path}: matches none of anyOf"));
        }
    }
    if let Some(ty) = schema.get("type") {
        let ok = match ty {
            Value::String(t) => type_matches(value, t),
            Value::Array(ts) => ts.iter().any(|t| type_matches(value, t.as_str().unwrap())),
            _ => true,
        };
        if !ok {
            errors.push(format!("{path}: expected type {ty}, got {value}"));
            return;
        }
    }
    if let Some(expected) = schema.get("const")
        && expected != value
    {
        errors.push(format!("{path}: expected {expected}, got {value}"));
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        errors.push(format!("{path}: {value} is not one of {options:?}"));
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required {
                let key = key.as_str().unwrap();
                if !object.contains_key(key) {
                    errors.push(format!("{path}: missing required field `{key}`"));
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (key, child) in object {
            let child_path = format!("{path}.{key}");
            if let Some(sub) = properties.and_then(|p| p.get(key)) {
                validate(root, sub, child, &child_path, errors);
            } else if let Some(extra) = schema.get("additionalProperties")
                && extra.is_object()
            {
                validate(root, extra, child, &child_path, errors);
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
        for (i, item) in array.iter().enumerate() {
            validate(root, items, item, &format!("{path}[{i}]"), errors);
        }
    }
}

pub(crate) fn assert_valid(document: &Value, def: &str, what: &str) {
    let root = schema();
    let schema = resolve(&root, &format!("#/$defs/{def}"));
    let mut errors = Vec::new();
    validate(&root, schema, document, "$", &mut errors);
    assert!(
        errors.is_empty(),
        "{what}: document does not match $defs/{def}:\n  {}\ndocument: {document}",
        errors.join("\n  ")
    );
    assert_eq!(
        document["schema_version"], SCHEMA_VERSION,
        "{what}: schema_version"
    );
}

// ---------------------------------------------------------------------------
// Shape snapshots: field names and JSON types, with data-dependent keys
// (file names, hashes, line ranges, tool::model) collapsed to `<key>`.
// ---------------------------------------------------------------------------

fn is_dynamic_key(key: &str) -> bool {
    let fixed = key
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && key.starts_with(|c: char| c.is_ascii_lowercase());
    let hash_like = key.len() >= 12 && key.chars().all(|c| c.is_ascii_hexdigit());
    !fixed || hash_like || key.starts_with("s_") || key.starts_with("h_")
}

fn shape(value: &Value) -> Value {
    match value {
        Value::Null => Value::String("null".into()),
        Value::Bool(_) => Value::String("boolean".into()),
        Value::Number(n) if n.is_f64() => Value::String("number".into()),
        Value::Number(_) => Value::String("integer".into()),
        Value::String(_) => Value::String("string".into()),
        Value::Array(items) => match items.first() {
            Some(first) => Value::Array(vec![shape(first)]),
            None => Value::Array(vec![]),
        },
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                // Its optional fields depend on this machine's upload state;
                // the schema check covers it instead.
                if key == "cloud_sync" {
                    out.insert(key.clone(), Value::String("<cloud_sync>".into()));
                    continue;
                }
                let key = if is_dynamic_key(key) {
                    "<key>"
                } else {
                    key.as_str()
                };
                out.entry(key.to_string()).or_insert_with(|| shape(child));
            }
            Value::Object(out)
        }
    }
}

fn shape_text(document: &Value) -> String {
    serde_json::to_string_pretty(&shape(document)).unwrap()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A repo whose last commit has one human and two AI lines, and a second-to-
/// last commit with an untracked line.
pub(crate) fn repo_with_ai_commit() -> TestRepo {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "untracked base\n").unwrap();
    repo.stage_all_and_commit("Base").unwrap();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["human line", "ai one".ai(), "ai two".ai()]);
    repo.stage_all_and_commit("AI work").unwrap();
    repo
}

/// A repo whose last commit was typed by hand while capture was working: no
/// checkpoint at all, but autter recorded the commit. Its lines are untracked.
fn repo_with_hand_typed_commit() -> TestRepo {
    let repo = TestRepo::new();
    fs::write(repo.path().join("first.txt"), "first\n").unwrap();
    repo.stage_all_and_commit("First").unwrap();
    let mut first = repo.filename("first.txt");
    first.assert_committed_lines(crate::lines!["first".unattributed_human()]);
    fs::write(repo.path().join("plain.txt"), "typed by hand\nno agent\n").unwrap();
    repo.stage_all_and_commit("Typed by hand").unwrap();
    let mut plain = repo.filename("plain.txt");
    plain.assert_committed_lines(crate::lines![
        "typed by hand".unattributed_human(),
        "no agent".unattributed_human(),
    ]);
    repo
}

/// A repo whose last commit capture missed: made with plain git, which the
/// background service never sees, so the commit has no authorship record. Its
/// lines are unknown.
fn repo_with_unrecorded_commit() -> TestRepo {
    let repo = TestRepo::new();
    fs::write(repo.path().join("first.txt"), "first\n").unwrap();
    repo.stage_all_and_commit("First").unwrap();
    let mut first = repo.filename("first.txt");
    first.assert_committed_lines(crate::lines!["first".unattributed_human()]);
    fs::write(
        repo.path().join("plain.txt"),
        "capture missed\nthis commit\n",
    )
    .unwrap();
    repo.git_og(&["add", "-A"]).unwrap();
    repo.git_og(&["commit", "-m", "Made while autter was not recording"])
        .unwrap();
    let head = repo.git_og(&["rev-parse", "HEAD"]).unwrap();
    assert!(
        repo.read_authorship_note(head.trim()).is_none(),
        "nothing observed the commit, so it has no record"
    );
    repo
}

/// An address nothing listens on: commands run with it see the background
/// service as not running.
const SERVICE_DOWN: (&str, &str) = (
    "AUTTER_DAEMON_CONTROL_SOCKET",
    "/nonexistent/autter-test/control.sock",
);

// ---------------------------------------------------------------------------
// 1. Every reporting command: --json, one document, schema, snapshot
// ---------------------------------------------------------------------------

/// (snapshot name, schema $def, args, allowed exit codes)
const REPORTING_COMMANDS: &[(&str, &str, &[&str], &[i32])] = &[
    ("stats", "stats", &["stats", "--json"], &[0]),
    (
        "stats_leading_json",
        "stats",
        &["--json", "stats", "HEAD"],
        &[0],
    ),
    (
        "stats_range",
        "stats_range",
        &["stats", "HEAD~1..HEAD", "--json"],
        &[0],
    ),
    ("status", "status", &["status", "--json"], &[0]),
    ("log", "log", &["log", "--json"], &[0]),
    ("show", "show", &["show", "HEAD", "--json"], &[0]),
    (
        "show_range",
        "show",
        &["show", "HEAD~1..HEAD", "--json"],
        &[0],
    ),
    ("blame", "blame", &["blame", "app.txt", "--json"], &[0]),
    ("diff", "diff", &["diff", "HEAD", "--json"], &[0]),
    ("whoami", "whoami", &["whoami", "--json"], &[0, 1]),
    (
        "sync_status",
        "sync_status",
        &["sync", "status", "--json"],
        &[0, 1],
    ),
    ("verify", "verify", &["verify", "HEAD", "--json"], &[0]),
];

#[test]
fn every_reporting_command_prints_one_valid_versioned_document() {
    let repo = repo_with_ai_commit();
    for (name, def, args, allowed) in REPORTING_COMMANDS {
        let result = run(&repo, args, &[]);
        assert!(
            allowed.contains(&result.code),
            "autter {}: exit {} not in {allowed:?}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            result.code,
            result.stdout,
            result.stderr
        );
        let document = parse_document(&result, name);
        assert_valid(&document, def, name);
        // A non-zero exit always carries an error object with the same code.
        if result.code != 0 {
            assert_eq!(
                document["error"]["exit_code"], result.code,
                "{name}: error.exit_code must match the process exit code: {document}"
            );
        } else {
            assert!(
                document.get("error").is_none(),
                "{name}: unexpected error: {document}"
            );
        }
        insta::assert_snapshot!(format!("json_contract_shape_{name}"), shape_text(&document));
    }
}

#[test]
fn stats_json_reports_the_four_labels_and_capture() {
    let repo = repo_with_ai_commit();
    let document = parse_document(&run(&repo, &["stats", "--json"], &[]), "stats");
    assert_eq!(document["lines"]["ai"], 2, "{document}");
    assert_eq!(document["lines"]["added"], 3, "{document}");
    assert_eq!(document["capture"]["status"], "captured");
    assert_eq!(document["capture"]["note_present"], true);
    assert_eq!(document["percent"]["ai"], 66.7);
    // The original fields are still there for existing readers.
    assert_eq!(document["ai_additions"], 2);

    // Typed by hand with capture working: recorded, so its lines are
    // untracked and capture did not miss it.
    let hand_typed = repo_with_hand_typed_commit();
    let document = parse_document(&run(&hand_typed, &["stats", "--json"], &[]), "stats");
    assert_eq!(document["capture"]["status"], "captured", "{document}");
    assert_eq!(document["capture"]["note_present"], true, "{document}");
    assert_eq!(document["record"]["status"], "recorded", "{document}");
    assert_eq!(document["lines"]["ai"], 0);
    assert_eq!(document["lines"]["human"], 0);
    assert_eq!(document["lines"]["untracked"], 2, "{document}");
    assert_eq!(document["lines"]["unknown"], 0, "{document}");

    // No record at all: the lines are unknown and nothing was captured.
    let unrecorded = repo_with_unrecorded_commit();
    let document = parse_document(&run(&unrecorded, &["stats", "--json"], &[]), "stats");
    assert_eq!(
        document["capture"]["status"], "nothing_captured",
        "{document}"
    );
    assert_eq!(document["capture"]["note_present"], false, "{document}");
    assert_ne!(document["record"]["status"], "recorded", "{document}");
    assert_eq!(document["lines"]["untracked"], 0, "{document}");
    assert_eq!(document["lines"]["unknown"], 2, "{document}");
}

#[test]
fn doctor_json_is_valid_and_strict_exits_4_when_capture_is_broken() {
    // Doctor's end-to-end checks drive the background service; keep them off
    // the daemon other tests share.
    let repo = TestRepo::new_dedicated_daemon();
    let plain = run(&repo, &["doctor", "--json"], &[]);
    let report = parse_document(&plain, "doctor");
    assert_valid(&report, "doctor", "doctor");
    insta::assert_snapshot!("json_contract_shape_doctor_summary", {
        let mut keys: Vec<&String> = report.as_object().unwrap().keys().collect();
        keys.retain(|k| k.as_str() != "error" && k.as_str() != "next_step");
        format!("{keys:?}")
    });

    let ok = report["ok"].as_bool().unwrap();
    let capture_ok = report["capture_ok"].as_bool().unwrap();
    assert_eq!(plain.code, if ok { 0 } else { 1 }, "{report}");

    let strict = run(&repo, &["doctor", "--json", "--strict"], &[]);
    let strict_report = parse_document(&strict, "doctor --strict");
    assert_valid(&strict_report, "doctor", "doctor --strict");
    let expected = if !strict_report["capture_ok"].as_bool().unwrap() {
        4
    } else if strict_report["ok"].as_bool().unwrap() {
        0
    } else {
        1
    };
    assert_eq!(strict.code, expected, "{strict_report}");
    if expected == 4 {
        assert_eq!(strict_report["error"]["code"], "capture_broken");
    }
    // Default behaviour is unchanged: broken capture alone never exits 4.
    if !capture_ok {
        assert_eq!(plain.code, 1);
    }
}

// ---------------------------------------------------------------------------
// 2. Exit codes
// ---------------------------------------------------------------------------

/// Exit 3 means capture missed the commit (no authorship record), not that
/// the commit has no AI lines.
#[test]
fn stats_strict_exits_3_only_when_nothing_was_captured() {
    let uncaptured = repo_with_unrecorded_commit();
    let captured = repo_with_ai_commit();

    // A commit typed by hand, with no agent and no editor extension, on a
    // machine where capture works: it has a record, so it passes.
    let hand_typed = repo_with_hand_typed_commit();
    for args in [
        &["stats", "--strict"][..],
        &["stats", "--json", "--strict"][..],
        &["stats", "HEAD", "--strict"][..],
    ] {
        let result = run(&hand_typed, args, &[]);
        assert_eq!(
            result.code, 0,
            "{args:?} on a recorded hand-typed commit: {}{}",
            result.stdout, result.stderr
        );
    }
    assert_eq!(
        run(&hand_typed, &["stats"], &[("AUTTER_STRICT", "1")]).code,
        0
    );

    // Default behaviour is unchanged.
    assert_eq!(run(&uncaptured, &["stats"], &[]).code, 0);
    assert_eq!(run(&uncaptured, &["stats", "--json"], &[]).code, 0);

    let strict = run(&uncaptured, &["stats", "--strict"], &[]);
    assert_eq!(strict.code, 3, "stderr: {}", strict.stderr);
    assert!(
        strict.stderr.contains("nothing captured"),
        "{}",
        strict.stderr
    );
    assert!(
        strict.stdout.contains('%'),
        "the human summary is still printed"
    );

    // `--strict` before the subcommand, and AUTTER_STRICT=1, do the same.
    assert_eq!(run(&uncaptured, &["--strict", "stats"], &[]).code, 3);
    assert_eq!(
        run(&uncaptured, &["stats"], &[("AUTTER_STRICT", "1")]).code,
        3
    );
    assert_eq!(
        run(&uncaptured, &["stats"], &[("AUTTER_STRICT", "0")]).code,
        0
    );

    assert_eq!(run(&captured, &["stats", "--strict"], &[]).code, 0);
    assert_eq!(
        run(&captured, &["stats", "--json", "--strict"], &[]).code,
        0
    );
}

#[test]
fn stats_json_strict_prints_a_json_error_object() {
    let repo = repo_with_unrecorded_commit();
    let result = run(&repo, &["stats", "--json", "--strict"], &[]);
    assert_eq!(result.code, 3);
    let document = parse_document(&result, "stats --json --strict");
    assert_valid(&document, "stats", "stats --json --strict");
    assert_eq!(document["error"]["code"], "nothing_captured");
    assert_eq!(document["error"]["exit_code"], 3);
    assert!(document["error"]["message"].is_string());
    // The report is kept next to the error.
    assert_eq!(document["capture"]["status"], "nothing_captured");
    assert_eq!(document["lines"]["unknown"], 2, "{document}");
}

#[test]
fn stats_strict_passes_a_commit_with_no_additions() {
    let repo = repo_with_hand_typed_commit();
    repo.git(&["commit", "--allow-empty", "-m", "empty"])
        .unwrap();
    let result = run(&repo, &["stats", "--json", "--strict"], &[]);
    assert_eq!(result.code, 0, "{}{}", result.stdout, result.stderr);
    let document = parse_document(&result, "stats empty");
    assert_eq!(document["capture"]["status"], "no_additions");
}

#[test]
fn stats_range_strict() {
    // Every added line in the range comes from a commit with no record.
    let repo = repo_with_unrecorded_commit();
    let result = run(&repo, &["stats", "HEAD~1..HEAD", "--json", "--strict"], &[]);
    assert_eq!(result.code, 3, "{}{}", result.stdout, result.stderr);
    let document = parse_document(&result, "range strict");
    assert_valid(&document, "stats_range", "range strict");
    assert_eq!(document["error"]["code"], "nothing_captured");
    assert_eq!(document["lines"]["unknown"], 2, "{document}");

    let captured = repo_with_ai_commit();
    assert_eq!(
        run(&captured, &["stats", "HEAD~1..HEAD", "--strict"], &[]).code,
        0
    );

    // A range of recorded commits typed by hand passes: capture worked.
    let hand_typed = repo_with_hand_typed_commit();
    let result = run(
        &hand_typed,
        &["stats", "HEAD~1..HEAD", "--json", "--strict"],
        &[],
    );
    assert_eq!(result.code, 0, "{}{}", result.stdout, result.stderr);
    let document = parse_document(&result, "hand-typed range");
    assert_eq!(document["capture"]["status"], "captured", "{document}");
    assert_eq!(document["lines"]["untracked"], 2, "{document}");
}

/// `stats --strict` on a merge commit judges the lines the merge itself adds:
/// none for a clean merge (pass), the conflict resolution otherwise.
#[test]
fn stats_strict_judges_a_merge_commit_on_its_own_lines() {
    let repo = TestRepo::new();
    let path = repo.path().join("app.txt");
    fs::write(&path, "shared\nline two\n").unwrap();
    repo.stage_all_and_commit("Base").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines![
        "shared".unattributed_human(),
        "line two".unattributed_human(),
    ]);
    let main = repo.current_branch();

    // A clean merge: the branch only adds a file of its own.
    repo.git(&["checkout", "-b", "clean"]).unwrap();
    fs::write(repo.path().join("side.txt"), "side\n").unwrap();
    repo.stage_all_and_commit("Side file").unwrap();
    let mut side = repo.filename("side.txt");
    side.assert_committed_lines(crate::lines!["side".unattributed_human()]);
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.txt"), "main\n").unwrap();
    repo.stage_all_and_commit("Main file").unwrap();
    repo.git(&["merge", "--no-ff", "-m", "Clean merge", "clean"])
        .unwrap();
    side.assert_committed_lines(crate::lines!["side".unattributed_human()]);

    let clean = run(&repo, &["stats", "--json", "--strict"], &[]);
    assert_eq!(clean.code, 0, "{}{}", clean.stdout, clean.stderr);
    let document = parse_document(&clean, "clean merge");
    assert_eq!(document["capture"]["status"], "no_additions", "{document}");
    assert_eq!(document["lines"]["added"], 0, "{document}");

    // A conflicting merge: both sides rewrite line two.
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&path, "shared\nfeature two\n").unwrap();
    repo.stage_all_and_commit("Feature rewrites line two")
        .unwrap();
    file.assert_committed_lines(crate::lines![
        "shared".unattributed_human(),
        "feature two".unattributed_human(),
    ]);
    repo.git(&["checkout", &main]).unwrap();
    fs::write(&path, "shared\nmain two\n").unwrap();
    repo.stage_all_and_commit("Main rewrites line two").unwrap();
    file.assert_committed_lines(crate::lines![
        "shared".unattributed_human(),
        "main two".unattributed_human(),
    ]);
    assert!(
        repo.git(&["merge", "feature"]).is_err(),
        "both branches changed line two, so the merge conflicts"
    );

    // The resolution is a line neither parent has. It is committed with plain
    // git, so capture misses the merge commit.
    fs::write(&path, "shared\nresolved by hand\n").unwrap();
    repo.git_og(&["add", "app.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Merge feature, resolved"])
        .unwrap();
    let merge = repo.git_og(&["rev-parse", "HEAD"]).unwrap();
    assert!(repo.read_authorship_note(merge.trim()).is_none());

    let missed = run(&repo, &["stats", "--json", "--strict"], &[]);
    assert_eq!(missed.code, 3, "{}{}", missed.stdout, missed.stderr);
    let document = parse_document(&missed, "unrecorded merge");
    assert_eq!(document["error"]["code"], "nothing_captured", "{document}");
    assert_eq!(document["lines"]["added"], 1, "{document}");
    assert_eq!(document["lines"]["unknown"], 1, "{document}");
    assert_eq!(
        run(&repo, &["stats", "--json"], &[]).code,
        0,
        "without --strict nothing changes"
    );
}

/// A merge whose conflict an agent resolved: the merge's own line is AI.
#[test]
fn stats_counts_an_ai_conflict_resolution_in_a_merge_commit() {
    let repo = TestRepo::new();
    let path = repo.path().join("app.txt");
    fs::write(&path, "shared\nline two\n").unwrap();
    repo.stage_all_and_commit("Base").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines![
        "shared".unattributed_human(),
        "line two".unattributed_human(),
    ]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&path, "shared\nfeature two\n").unwrap();
    repo.stage_all_and_commit("Feature rewrites line two")
        .unwrap();
    file.assert_committed_lines(crate::lines![
        "shared".unattributed_human(),
        "feature two".unattributed_human(),
    ]);
    repo.git(&["checkout", &main]).unwrap();
    fs::write(&path, "shared\nmain two\n").unwrap();
    repo.stage_all_and_commit("Main rewrites line two").unwrap();
    file.assert_committed_lines(crate::lines![
        "shared".unattributed_human(),
        "main two".unattributed_human(),
    ]);
    assert!(repo.git(&["merge", "feature"]).is_err());

    // The agent writes a resolution neither side had. Staged first: a
    // checkpoint skips files git still lists as unmerged.
    fs::write(&path, "shared\nresolved by the agent\n").unwrap();
    repo.git(&["add", "app.txt"]).unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.stage_all_and_commit("Merge feature, resolved by AI")
        .unwrap();
    file.assert_committed_lines(crate::lines![
        "shared".unattributed_human(),
        "resolved by the agent".ai(),
    ]);

    let result = run(&repo, &["stats", "--json", "--strict"], &[]);
    assert_eq!(result.code, 0, "{}{}", result.stdout, result.stderr);
    let document = parse_document(&result, "AI-resolved merge");
    assert_eq!(document["capture"]["status"], "captured", "{document}");
    assert_eq!(document["lines"]["added"], 1, "{document}");
    assert_eq!(document["lines"]["ai"], 1, "{document}");
}

/// `status --strict` follows the same rule as `stats --strict`: exit 3 only
/// when a commit of the working tree would get no record.
#[test]
fn status_strict_exits_3_for_uncaptured_changes() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("tracked.txt"), "one\n").unwrap();
    repo.stage_all_and_commit("Initial").unwrap();
    let mut tracked = repo.filename("tracked.txt");
    tracked.assert_committed_lines(crate::lines!["one".unattributed_human()]);

    // Clean tree: nothing to capture.
    let clean = run(&repo, &["status", "--json", "--strict"], &[]);
    assert_eq!(clean.code, 0, "{}{}", clean.stdout, clean.stderr);
    assert_eq!(
        parse_document(&clean, "status clean")["capture"]["status"],
        "no_additions"
    );

    // A change typed by hand, which no checkpoint saw, while the background
    // service is running: the commit it goes into will be recorded. Passes.
    fs::write(repo.path().join("tracked.txt"), "one\ntwo\nthree\n").unwrap();
    assert_eq!(run(&repo, &["status"], &[]).code, 0, "default unchanged");
    let typed = run(&repo, &["status", "--json", "--strict"], &[]);
    assert_eq!(typed.code, 0, "{}{}", typed.stdout, typed.stderr);
    let document = parse_document(&typed, "status hand-typed");
    assert_valid(&document, "status", "status hand-typed");
    assert_eq!(document["capture"]["status"], "captured", "{document}");
    assert_eq!(document["lines"]["untracked"], 2, "{document}");
    assert_eq!(document["lines"]["unknown"], 0, "{document}");

    // The same change with the background service not running: a commit made
    // now would get no record.
    assert_eq!(
        run(&repo, &["status"], &[SERVICE_DOWN]).code,
        0,
        "default unchanged"
    );
    let strict = run(&repo, &["status", "--json", "--strict"], &[SERVICE_DOWN]);
    assert_eq!(strict.code, 3, "{}{}", strict.stdout, strict.stderr);
    let document = parse_document(&strict, "status strict");
    assert_valid(&document, "status", "status strict");
    assert_eq!(document["error"]["code"], "nothing_captured");
    assert_eq!(document["lines"]["unknown"], 2, "{document}");
    assert_eq!(document["lines"]["untracked"], 0, "{document}");

    // Once an AI checkpoint covers it, strict passes.
    repo.autter(&["checkpoint", "mock_ai", "tracked.txt"])
        .unwrap();
    let captured = run(&repo, &["status", "--json", "--strict"], &[]);
    assert_eq!(captured.code, 0, "{}{}", captured.stdout, captured.stderr);
    assert_eq!(
        parse_document(&captured, "status captured")["capture"]["status"],
        "captured"
    );
    // A checkpoint is a record whether or not the service is up right now.
    assert_eq!(run(&repo, &["status", "--strict"], &[SERVICE_DOWN]).code, 0);
}

/// A working tree whose only change is a new file git does not track yet is
/// still a change: `status --strict` must not treat it as a clean tree.
#[test]
fn status_strict_sees_files_git_does_not_track_yet() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("tracked.txt"), "one\n").unwrap();
    repo.stage_all_and_commit("Initial").unwrap();
    let mut tracked = repo.filename("tracked.txt");
    tracked.assert_committed_lines(crate::lines!["one".unattributed_human()]);

    // A new file, never `git add`ed, with nothing recording.
    fs::write(repo.path().join("new.txt"), "brand\nnew\nfile\n").unwrap();
    let missed = run(&repo, &["status", "--json", "--strict"], &[SERVICE_DOWN]);
    assert_eq!(missed.code, 3, "{}{}", missed.stdout, missed.stderr);
    let document = parse_document(&missed, "new file, service down");
    assert_eq!(document["lines"]["added"], 3, "{document}");
    assert_eq!(document["lines"]["unknown"], 3, "{document}");
    assert_eq!(document["stats"]["git_diff_added_lines"], 3, "{document}");

    // The same file written by an agent: captured.
    repo.autter(&["checkpoint", "mock_ai", "new.txt"]).unwrap();
    let captured = run(&repo, &["status", "--json", "--strict"], &[SERVICE_DOWN]);
    assert_eq!(captured.code, 0, "{}{}", captured.stdout, captured.stderr);
    let document = parse_document(&captured, "new AI file");
    assert_eq!(document["capture"]["status"], "captured", "{document}");
    assert_eq!(document["lines"]["added"], 3, "{document}");
    assert_eq!(document["lines"]["ai"], 3, "{document}");
    assert_eq!(document["stats"]["git_diff_added_lines"], 3, "{document}");
}

#[test]
fn outside_a_repository_is_a_usage_error_with_a_json_error() {
    let repo = TestRepo::new();
    let outside = tempfile::tempdir().unwrap();
    for args in [
        &["stats", "--json"][..],
        &["status", "--json"],
        &["log", "--json"],
        &["show", "HEAD", "--json"],
        &["blame", "file.txt", "--json"],
        &["diff", "HEAD", "--json"],
    ] {
        let mut command = repo.autter_command(args);
        command.current_dir(outside.path());
        command.env("GIT_CEILING_DIRECTORIES", outside.path());
        let result = run_output(command.output().unwrap());
        assert_eq!(
            result.code,
            2,
            "autter {}: {}{}",
            args.join(" "),
            result.stdout,
            result.stderr
        );
        let document = parse_document(&result, args[0]);
        assert_valid(&document, "error_only", args[0]);
        assert_eq!(document["error"]["code"], "not_a_repository", "{document}");
    }
}

#[test]
fn bad_flags_are_usage_errors_with_a_json_error() {
    let repo = repo_with_ai_commit();
    for args in [
        &["stats", "--bogus", "--json"][..],
        &["status", "--bogus", "--json"],
        &["show", "--bogus", "--json"],
        &["whoami", "--bogus", "--json"],
        &["doctor", "--bogus", "--json"],
        &["log", "--json", "--graph"],
    ] {
        let result = run(&repo, args, &[]);
        assert_eq!(
            result.code,
            2,
            "autter {}: {}{}",
            args.join(" "),
            result.stdout,
            result.stderr
        );
        let document = parse_document(&result, args[0]);
        assert_valid(&document, "error_only", args[0]);
        assert_eq!(document["error"]["code"], "usage_error", "{document}");
    }
}

#[test]
fn an_unknown_revision_is_a_runtime_error_with_a_json_error() {
    let repo = repo_with_ai_commit();
    let result = run(&repo, &["stats", "does-not-exist", "--json"], &[]);
    assert_eq!(result.code, 1, "{}{}", result.stdout, result.stderr);
    let document = parse_document(&result, "stats bad rev");
    assert_valid(&document, "error_only", "stats bad rev");
    assert_eq!(document["error"]["code"], "revision_not_found");
}

#[test]
fn json_errors_are_one_document_on_stdout_and_text_on_stderr() {
    let repo = repo_with_unrecorded_commit();
    let result = run(&repo, &["stats", "--json", "--strict"], &[]);
    assert_eq!(result.stdout.trim().lines().count(), 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("nothing captured"),
        "{}",
        result.stderr
    );
    assert!(!result.stdout.contains("\x1b["), "no ANSI escapes in JSON");
}

#[test]
fn verify_failure_exits_5_and_keeps_the_report() {
    let repo = repo_with_ai_commit();
    let written_for = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    repo.git_og(&["commit", "--allow-empty", "-m", "empty"])
        .unwrap();
    repo.git_og(&["notes", "--ref=ai", "copy", &written_for, "HEAD"])
        .unwrap();
    let result = run(&repo, &["verify", "HEAD", "--json"], &[]);
    assert_eq!(result.code, 5, "{}\n{}", result.stdout, result.stderr);
    let document = parse_document(&result, "verify_failed");
    assert_valid(&document, "verify", "verify_failed");
    assert_eq!(document["error"]["code"], "integrity_failed");
    assert_eq!(document["error"]["exit_code"], 5);
    assert_eq!(document["results"][0]["status"], "wrong_commit");
    assert_eq!(document["failed"], 1);
}

#[test]
fn help_documents_exit_codes_and_strict() {
    let repo = TestRepo::new();
    let result = run(&repo, &["--help"], &[]);
    assert_eq!(result.code, 0);
    let help = format!("{}{}", result.stdout, result.stderr);
    for needle in [
        "Exit codes:",
        "Nothing captured",
        "Capture is broken",
        "integrity check",
        "--strict",
        "AUTTER_STRICT",
    ] {
        assert!(
            help.contains(needle),
            "`autter --help` lacks {needle}:\n{help}"
        );
    }
}

#[test]
fn ci_mode_prints_no_color_or_spinners() {
    let repo = repo_with_ai_commit();
    let result = run(&repo, &["stats"], &[("CI", "true"), ("FORCE_COLOR", "1")]);
    assert_eq!(result.code, 0);
    assert!(!result.stdout.contains("\x1b["), "{:?}", result.stdout);
    assert!(!result.stderr.contains("\x1b["), "{:?}", result.stderr);
}
