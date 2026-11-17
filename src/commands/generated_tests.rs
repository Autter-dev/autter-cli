//! `autter tests`: the tests Autter wrote for a pull request, on your machine.
//!
//! On a review, Autter writes tests for the changed code it found untested,
//! runs them on the PR's base and head in its sandbox, and keeps the ones that
//! say something: passing tests that pin behaviour, and failing tests that
//! reproduce a bug the PR introduces. Failing ones are never committed (they
//! would turn CI red), so this command brings them to the developer:
//!
//! * `list`  — what Autter wrote for the PR, which failed and why.
//! * `pull`  — write the test files into this checkout.
//! * `run`   — pull, then run each file with its framework's command.
//! * `clean` — remove the files `pull` wrote (and only those).
//!
//! Safety rules, because the files are model-written from the PR's code and
//! run here with your environment instead of Autter's sandbox:
//!
//! * The command that runs a file is built here from its framework and path;
//!   the command string the server stored is never executed.
//! * `run` asks before running (`--yes` skips the prompt; without a terminal
//!   it refuses instead of assuming yes).
//! * A test that ran against Autter's temporary database runs here only with
//!   an explicit `--database-url`: it was told it owns an empty database, so it
//!   must never inherit a `DATABASE_URL` that points at a real one.
//! * `pull` never overwrites a file it did not write. What it wrote is
//!   recorded under `.git/ai/generated-tests/`, so `clean` removes exactly that.
use crate::commands::arg_parser::{self, ScanMode};
use crate::commands::ask::{component, org_api_context};
use crate::commands::memory::{response_json_or, sharing_refusal, target_repository};
use crate::config::Config;
use crate::error::AutterError;
use crate::git::repository::{Repository, discover_repository_in_path_no_git_exec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const MISSING_ROUTE: &str = "This Autter deployment does not serve generated tests to the CLI yet.";
const SUBCOMMANDS: &str = "list, pull, run or clean";
/// Env names a test that used Autter's temporary database gets when the
/// stored result does not list them (results stored before they were recorded).
const DEFAULT_DATABASE_NAMES: &[&str] = &["AUTTER_TEST_DATABASE_URL", "DATABASE_URL"];

#[derive(Debug, Default, PartialEq)]
struct Options {
    subcommand: String,
    pr: Option<u64>,
    repository: Option<String>,
    organization: Option<String>,
    /// Passing tests too (default: only the failing ones).
    all: bool,
    /// Only these tests (a test file path or a gap id such as `src/svc.ts#discount`).
    only: Vec<String>,
    database_url: Option<String>,
    yes: bool,
    dry_run: bool,
    /// Replace or remove a file `pull` wrote earlier even though it was edited since.
    force: bool,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if !arg.starts_with("--") {
            if options.subcommand.is_empty() {
                options.subcommand = arg.to_owned();
            } else if options.pr.is_none() && arg.trim_start_matches('#').parse::<u64>().is_ok() {
                options.pr = Some(parse_pr(arg)?);
            } else {
                return Err(format!("unexpected argument: {arg}"));
            }
            index += 1;
            continue;
        }
        match arg {
            "--all" => options.all = true,
            "--yes" | "-y" => options.yes = true,
            "--dry-run" => options.dry_run = true,
            "--force" => options.force = true,
            _ => {
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.starts_with("--"))
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                match arg {
                    "--pr" => options.pr = Some(parse_pr(value)?),
                    "--repo" => options.repository = Some(value.clone()),
                    "--org" => options.organization = Some(value.clone()),
                    "--only" => options.only.push(value.clone()),
                    "--database-url" => options.database_url = Some(value.clone()),
                    _ => return Err(format!("unknown option for tests: {arg}")),
                }
                index += 1;
            }
        }
        index += 1;
    }
    let sub = options.subcommand.as_str();
    if sub.is_empty() {
        return Err(format!("tests needs a subcommand: {SUBCOMMANDS}"));
    }
    if !matches!(sub, "list" | "pull" | "run" | "clean") {
        return Err(format!("unknown tests subcommand: {sub}"));
    }
    if options.database_url.is_some() && sub != "run" {
        return Err("--database-url only applies to tests run".into());
    }
    if (options.yes || options.dry_run) && sub != "run" {
        return Err(format!(
            "{} only applies to tests run",
            if options.yes { "--yes" } else { "--dry-run" }
        ));
    }
    if sub == "clean" && (options.all || !options.only.is_empty()) {
        return Err(
            "tests clean removes every file pull wrote; it takes no --all or --only".into(),
        );
    }
    if sub == "list" && options.force {
        return Err("tests list writes nothing, so it takes no --force".into());
    }
    if let Some(url) = &options.database_url {
        database_url_parts(url)?;
    }
    Ok(options)
}

fn parse_pr(value: &str) -> Result<u64, String> {
    value
        .trim_start_matches('#')
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("--pr must be a pull request number, not {value}"))
}

// ── What the server sends ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeneratedTest {
    gap_id: String,
    path: String,
    #[serde(default)]
    dir: String,
    framework: String,
    /// The command Autter's sandbox used. Shown, never executed.
    #[serde(default)]
    command: String,
    source: String,
    #[serde(default)]
    cases: Vec<String>,
    status: String,
    #[serde(default)]
    behaviour: String,
    /// None on results stored before it was recorded.
    #[serde(default)]
    uses_database: Option<bool>,
    #[serde(default)]
    database_env_names: Vec<String>,
    #[serde(default)]
    env_names: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Failure {
    #[serde(default)]
    gap_id: String,
    test_path: String,
    #[serde(default)]
    test_name: String,
    #[serde(default)]
    explanation: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    output: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CouldNotRun {
    #[serde(default)]
    gap_id: Option<String>,
    reason: String,
    #[serde(default)]
    needs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Dropped {
    #[serde(default)]
    gap_id: String,
    path: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    detail: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeneratedTests {
    head_sha: String,
    #[serde(default)]
    base_sha: String,
    #[serde(default)]
    is_stale: bool,
    tests: Vec<GeneratedTest>,
    #[serde(default)]
    failures: Vec<Failure>,
    #[serde(default)]
    could_not_run: Vec<CouldNotRun>,
    #[serde(default)]
    dropped: Vec<Dropped>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PullRequest {
    number: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    head_ref: String,
    #[serde(default)]
    head_sha: String,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiResponse {
    repository: String,
    pull_request: PullRequest,
    #[serde(default)]
    generated_tests: Option<GeneratedTests>,
    #[serde(default)]
    message: Option<String>,
}

// ── Choosing tests ──────────────────────────────────────────────────────────

/// The tests a command acts on: `--only` matches, else all with `--all`, else
/// the failing ones.
fn select<'a>(tests: &'a [GeneratedTest], all: bool, only: &[String]) -> Vec<&'a GeneratedTest> {
    tests
        .iter()
        .filter(|t| {
            if !only.is_empty() {
                only.iter().any(|o| o == &t.path || o == &t.gap_id)
            } else {
                all || t.status == "failed"
            }
        })
        .collect()
}

// ── Paths and the pull manifest ─────────────────────────────────────────────

/// A path the server says to write: relative, `/`-separated, inside the tree,
/// not under `.git/` or `.github/`. Returns it as a native relative path.
fn safe_relative_path(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.contains('\\') || path.contains('\0') {
        return None;
    }
    let p = Path::new(path);
    let mut out = PathBuf::new();
    for component in p.components() {
        match component {
            Component::Normal(part) => out.push(part),
            _ => return None,
        }
    }
    let first = path.split('/').next().unwrap_or("");
    if first == ".git" || first == ".github" || out.as_os_str().is_empty() {
        return None;
    }
    Some(out)
}

/// A test's run directory (`""` = repository root), checked like a path.
fn safe_dir(dir: &str) -> Option<PathBuf> {
    if dir.is_empty() {
        Some(PathBuf::new())
    } else {
        safe_relative_path(dir)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    pr: u64,
    head_sha: String,
    /// Path → sha256 of the content `pull` wrote.
    files: BTreeMap<String, String>,
    /// Directories `pull` created (removed by `clean` when empty).
    #[serde(default)]
    dirs: Vec<String>,
}

fn manifest_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("ai").join("generated-tests")
}

fn manifest_path(git_dir: &Path, pr: u64) -> PathBuf {
    manifest_dir(git_dir).join(format!("pr-{pr}.json"))
}

fn load_manifest(git_dir: &Path, pr: u64) -> Manifest {
    std::fs::read_to_string(manifest_path(git_dir, pr))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Manifest {
            pr,
            ..Manifest::default()
        })
}

fn save_manifest(git_dir: &Path, manifest: &Manifest) -> Result<(), AutterError> {
    let path = manifest_path(git_dir, manifest.pr);
    if manifest.files.is_empty() && manifest.dirs.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }
    std::fs::create_dir_all(manifest_dir(git_dir))?;
    std::fs::write(&path, serde_json::to_string_pretty(manifest)?)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum WriteOutcome {
    Written,
    Unchanged,
    Refused { reason: String },
}

/// Write `test` into the checkout at `root`, recording it in `manifest`.
/// Never overwrites a file `pull` did not write, nor (without `force`) one it
/// wrote that was edited since.
fn write_test(
    root: &Path,
    test: &GeneratedTest,
    manifest: &mut Manifest,
    force: bool,
) -> WriteOutcome {
    let refuse = |reason: &str| WriteOutcome::Refused {
        reason: reason.to_owned(),
    };
    let Some(rel) = safe_relative_path(&test.path) else {
        return refuse("not a path inside the repository");
    };
    let target = root.join(&rel);
    let wanted = sha256_hex(test.source.as_bytes());
    if let Ok(existing) = std::fs::read(&target) {
        let current = sha256_hex(&existing);
        if current == wanted {
            manifest.files.entry(test.path.clone()).or_insert(wanted);
            return WriteOutcome::Unchanged;
        }
        match manifest.files.get(&test.path) {
            None => return refuse("a file Autter did not write is already there"),
            Some(written) if *written != current && !force => {
                return refuse("you edited it since it was pulled (--force replaces it)");
            }
            Some(_) => {}
        }
    }
    // Create missing parent directories one by one, so `clean` can remove them.
    let mut missing = Vec::new();
    let mut parent = rel.parent();
    while let Some(dir) = parent.filter(|d| !d.as_os_str().is_empty()) {
        if root.join(dir).exists() {
            break;
        }
        missing.push(dir.to_path_buf());
        parent = dir.parent();
    }
    for dir in missing.iter().rev() {
        if let Err(error) = std::fs::create_dir(root.join(dir)) {
            return refuse(&format!("could not create {}: {error}", dir.display()));
        }
        let posix = posix_path(dir);
        if !manifest.dirs.contains(&posix) {
            manifest.dirs.push(posix);
        }
    }
    if let Err(error) = std::fs::write(&target, &test.source) {
        return refuse(&format!("could not write it: {error}"));
    }
    manifest.files.insert(test.path.clone(), wanted);
    WriteOutcome::Written
}

fn posix_path(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug, Default, PartialEq, Serialize)]
struct CleanReport {
    removed: Vec<String>,
    /// Files edited since `pull` wrote them (kept unless `--force`).
    kept: Vec<String>,
}

/// Remove what `pull` wrote for one PR. Edited files stay (and stay recorded)
/// unless `force`.
fn clean_manifest(root: &Path, manifest: &mut Manifest, force: bool) -> CleanReport {
    let mut report = CleanReport::default();
    let mut remaining = BTreeMap::new();
    for (path, written) in std::mem::take(&mut manifest.files) {
        let Some(rel) = safe_relative_path(&path) else {
            continue;
        };
        let target = root.join(rel);
        match std::fs::read(&target) {
            Err(_) => {}
            Ok(content) if force || sha256_hex(&content) == written => {
                if std::fs::remove_file(&target).is_ok() {
                    report.removed.push(path);
                } else {
                    remaining.insert(path.clone(), written);
                    report.kept.push(path);
                }
            }
            Ok(_) => {
                remaining.insert(path.clone(), written);
                report.kept.push(path);
            }
        }
    }
    manifest.files = remaining;
    // Deepest first; a directory with anything else in it stays.
    let mut dirs = std::mem::take(&mut manifest.dirs);
    dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
    for dir in dirs {
        let Some(rel) = safe_relative_path(&dir) else {
            continue;
        };
        if std::fs::remove_dir(root.join(rel)).is_err() && root.join(&dir).exists() {
            manifest.dirs.push(dir);
        }
    }
    report
}

// ── Running ─────────────────────────────────────────────────────────────────

/// The program and arguments that run exactly `test`, from its `dir`. Built
/// from the framework and path (mirroring Autter's sandbox), never from the
/// command string the server stored. None for an unknown framework.
fn run_argv(test: &GeneratedTest, python: &str) -> Option<Vec<String>> {
    let dir = test.dir.trim_end_matches('/');
    let rel = if dir.is_empty() {
        test.path.as_str()
    } else {
        test.path.strip_prefix(&format!("{dir}/"))?
    };
    // Arguments are passed without a shell on Unix, but Windows runs them
    // through `cmd /C`; a generated file name never needs these characters.
    if rel.chars().any(|c| "&|<>^%\"'`$;!()\n\r".contains(c)) {
        return None;
    }
    let is_ts = Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e, "ts" | "tsx" | "mts" | "cts"));
    let argv: Vec<&str> = match test.framework.as_str() {
        "vitest" => vec!["npx", "vitest", "run", "--reporter=verbose", rel],
        "jest" => vec!["npx", "jest", "--ci", "--verbose", "--runTestsByPath", rel],
        "mocha" if is_ts => vec!["npx", "mocha", "--require", "tsx", rel],
        "mocha" => vec!["npx", "mocha", rel],
        "node-test" if is_ts => vec!["npx", "tsx", "--test", rel],
        "node-test" => vec!["node", "--test", rel],
        "pytest" => vec![
            python,
            "-m",
            "pytest",
            "-q",
            "-rf",
            "-p",
            "no:cacheprovider",
            rel,
        ],
        "go-test" => {
            let pkg = Path::new(rel)
                .parent()
                .map(posix_path)
                .filter(|p| !p.is_empty())
                .map(|p| format!("./{p}"))
                .unwrap_or_else(|| ".".into());
            return Some(
                [
                    "go",
                    "test",
                    "-count=1",
                    "-run",
                    "^TestAutter",
                    pkg.as_str(),
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            );
        }
        "cargo-test" => {
            let name = Path::new(rel).file_stem()?.to_str()?;
            vec!["cargo", "test", "--quiet", "--test", name]
        }
        "rspec" => vec!["bundle", "exec", "rspec", rel],
        "phpunit" => vec!["vendor/bin/phpunit", rel],
        _ => return None,
    };
    Some(argv.into_iter().map(str::to_owned).collect())
}

/// Shell-style rendering of an argv, for display only.
fn display_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./=:@".contains(c))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn on_path(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file() || (cfg!(windows) && dir.join(format!("{program}.exe")).is_file())
    })
}

fn python_program() -> &'static str {
    if !on_path("python") && on_path("python3") {
        "python3"
    } else {
        "python"
    }
}

const TEST_DB_PATTERN: &str = r"\b(?:AUTTER_TEST_DATABASE_URL|DATABASE_URL|POSTGRES_URL|PG(?:HOST|DATABASE|USER|PASSWORD|PORT))\b|\b(?:drizzle|prisma|knex|sequelize|typeorm|psycopg2?|asyncpg|sqlalchemy|sqlx::|diesel::|gorm\.Open)\b|new Pool\(";

/// Whether a test that predates the recorded flag talks to a database (its
/// source names a database variable or client).
fn source_reads_database(source: &str) -> bool {
    regex::Regex::new(TEST_DB_PATTERN)
        .map(|re| re.is_match(source))
        .unwrap_or(true)
}

fn uses_database(test: &GeneratedTest) -> bool {
    test.uses_database
        .unwrap_or_else(|| source_reads_database(&test.source))
}

/// Host, port, user, password and database name of a Postgres URL.
fn database_url_parts(url: &str) -> Result<[(String, String); 5], String> {
    let parsed = url::Url::parse(url).map_err(|_| {
        "--database-url must be a URL such as postgres://user:pass@localhost:5432/scratch"
            .to_owned()
    })?;
    if !matches!(parsed.scheme(), "postgres" | "postgresql") {
        return Err("--database-url must be a postgres:// URL".into());
    }
    let decode = |s: &str| {
        url::form_urlencoded::parse(format!("x={s}").as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    };
    Ok([
        ("HOST".into(), parsed.host_str().unwrap_or("").to_owned()),
        (
            "PORT".into(),
            parsed
                .port()
                .map(|p| p.to_string())
                .unwrap_or("5432".into()),
        ),
        ("USER".into(), decode(parsed.username())),
        ("PASSWORD".into(), decode(parsed.password().unwrap_or(""))),
        (
            "DATABASE".into(),
            parsed.path().trim_start_matches('/').to_owned(),
        ),
    ])
}

/// The database variables a test gets from `--database-url`: every name the
/// sandbox set, mapped the same way (URL aliases get the URL, PG* and
/// POSTGRES_* parts their piece).
fn database_env(test: &GeneratedTest, url: &str) -> Vec<(String, String)> {
    let parts = database_url_parts(url).unwrap_or_default();
    let part = |key: &str| {
        parts
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let mut names: Vec<String> = test.database_env_names.clone();
    for name in DEFAULT_DATABASE_NAMES {
        if !names.iter().any(|n| n == name) {
            names.push((*name).to_owned());
        }
    }
    names
        .into_iter()
        .map(|name| {
            let value = match name.as_str() {
                "PGHOST" | "POSTGRES_HOST" => part("HOST"),
                "PGPORT" | "POSTGRES_PORT" => part("PORT"),
                "PGUSER" | "POSTGRES_USER" => part("USER"),
                "PGPASSWORD" | "POSTGRES_PASSWORD" => part("PASSWORD"),
                "PGDATABASE" | "POSTGRES_DB" | "POSTGRES_DATABASE" => part("DATABASE"),
                _ => url.to_owned(),
            };
            (name, value)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum RunResult {
    Passed,
    Failed { exit_code: Option<i32> },
    Skipped { reason: String },
    NotStarted { reason: String },
}

/// Why `test` must not run here, if it must not.
fn skip_reason(test: &GeneratedTest, database_url: Option<&str>) -> Option<String> {
    if uses_database(test) && database_url.is_none() {
        return Some(
            "it ran against Autter's temporary database; pass --database-url with a scratch Postgres database it may write to (never a real one)".into(),
        );
    }
    if safe_dir(&test.dir).is_none() {
        return Some("its run directory is not inside the repository".into());
    }
    None
}

fn run_one(
    root: &Path,
    test: &GeneratedTest,
    argv: &[String],
    database_url: Option<&str>,
) -> RunResult {
    let Some(dir) = safe_dir(&test.dir) else {
        return RunResult::Skipped {
            reason: "its run directory is not inside the repository".into(),
        };
    };
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").args(argv);
        c
    } else {
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    };
    command.current_dir(root.join(dir)).env("CI", "true");
    if arg_parser::json() {
        // stdout carries exactly one JSON document; the runner's output goes to stderr.
        command.stdout(std::process::Stdio::from(std::io::stderr()));
    }
    if let Some(url) = database_url.filter(|_| uses_database(test)) {
        command.envs(database_env(test, url));
    }
    match command.status() {
        Ok(status) if status.success() => RunResult::Passed,
        Ok(status) => RunResult::Failed {
            exit_code: status.code(),
        },
        Err(error) => RunResult::NotStarted {
            reason: format!("could not start `{}`: {error}", argv[0]),
        },
    }
}

// ── Output ──────────────────────────────────────────────────────────────────

fn clean_text(text: &str) -> String {
    crate::commands::memory::clean(text)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn render_header(response: &ApiResponse, tests: &GeneratedTests) -> String {
    let pr = &response.pull_request;
    let failing = tests.tests.iter().filter(|t| t.status == "failed").count();
    let mut out = format!(
        "{} #{}{} ({})\n",
        clean_text(&response.repository),
        pr.number,
        pr.title
            .as_deref()
            .map(|t| format!(" — {}", clean_text(t)))
            .unwrap_or_default(),
        clean_text(&pr.head_ref),
    );
    out.push_str(&format!(
        "Autter wrote {} for this change on {}: {} passed, {} found a bug.\n",
        plural(tests.tests.len(), "test file", "test files"),
        short(&tests.head_sha),
        tests.tests.len() - failing,
        failing,
    ));
    if tests.is_stale {
        out.push_str(&format!(
            "The PR has moved on to {} since; the next review checks these tests again.\n",
            short(&pr.head_sha)
        ));
    }
    out
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn render_list(response: &ApiResponse, tests: &GeneratedTests) -> String {
    let mut out = render_header(response, tests);
    for test in &tests.tests {
        let mark = if test.status == "failed" {
            "✗"
        } else {
            "✓"
        };
        out.push_str(&format!(
            "\n{mark} {}  [{}]{}\n",
            clean_text(&test.path),
            clean_text(&test.framework),
            if uses_database(test) {
                "  (needs a database)"
            } else {
                ""
            }
        ));
        for failure in tests.failures.iter().filter(|f| f.test_path == test.path) {
            out.push_str(&format!(
                "    {} — {}\n    at {}\n",
                clean_text(&failure.test_name),
                clean_text(&failure.explanation),
                clean_text(&failure.location)
            ));
        }
        if test.status != "failed" {
            let what = if test.behaviour == "existing" {
                "pins behaviour that worked before this PR"
            } else {
                "covers what this PR adds"
            };
            out.push_str(&format!("    passes; {what}\n"));
        }
    }
    if !tests.could_not_run.is_empty() {
        out.push_str("\nCould not run in Autter's sandbox:\n");
        for c in &tests.could_not_run {
            out.push_str(&format!("  - {}\n", clean_text(&c.reason)));
        }
    }
    if !tests.dropped.is_empty() {
        out.push_str(&format!(
            "\n{} not kept (failed on the base too, flaky, or did not compile).\n",
            plural(tests.dropped.len(), "test was", "tests were")
        ));
    }
    let failing = tests.tests.iter().any(|t| t.status == "failed");
    out.push_str(&format!(
        "\nRun {} here with: autter tests run --pr {}{}\n",
        if failing { "the failing ones" } else { "them" },
        response.pull_request.number,
        if failing { "" } else { " --all" }
    ));
    out
}

// ── Commands ────────────────────────────────────────────────────────────────

/// A chosen test, the command that runs it (None when none can be built),
/// and why it must not run here (None when it may).
type Plan<'a> = (&'a GeneratedTest, Option<Vec<String>>, Option<String>);

struct Checkout {
    root: PathBuf,
    git_dir: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    repo: Repository,
}

fn checkout(local: Option<Repository>) -> Result<Checkout, AutterError> {
    let repo = local
        .ok_or_else(|| AutterError::Generic("Run this inside the repository's checkout.".into()))?;
    let head_ref = repo.head().ok();
    let head = head_ref.as_ref().and_then(|h| h.target().ok());
    let branch = head_ref
        .as_ref()
        .and_then(|h| h.shorthand().ok())
        .filter(|b| !b.is_empty() && b != "HEAD");
    Ok(Checkout {
        root: repo.workdir()?,
        git_dir: repo.path().to_path_buf(),
        head,
        branch,
        repo,
    })
}

fn fetch(
    options: &Options,
    repository: &str,
    branch: Option<&str>,
) -> Result<ApiResponse, AutterError> {
    let (context, slug) = org_api_context(options.organization.as_deref(), 60)?;
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    params.append_pair("repository", repository);
    match (options.pr, branch) {
        (Some(pr), _) => {
            params.append_pair("pr", &pr.to_string());
        }
        (None, Some(branch)) => {
            params.append_pair("branch", branch);
        }
        (None, None) => {
            return Err(AutterError::Generic(if options.repository.is_some() {
                "With --repo, name the pull request with --pr <number>.".into()
            } else {
                "This checkout is not on a branch. Pass --pr <number>.".into()
            }));
        }
    }
    let value = response_json_or(
        context.get(&format!(
            "/api/cli/orgs/{}/generated-tests?{}",
            component(&slug),
            params.finish()
        ))?,
        MISSING_ROUTE,
    )?;
    serde_json::from_value(value).map_err(|error| {
        AutterError::Generic(format!(
            "Autter sent generated tests this CLI cannot read ({error}). Run `autter upgrade`."
        ))
    })
}

/// Warn when this checkout is not at the commit the tests were checked on.
fn head_notice(local: &Checkout, tests: &GeneratedTests, pr: &PullRequest) -> Option<String> {
    let head = local.head.as_deref()?;
    if head == tests.head_sha {
        return None;
    }
    let known = local.repo.find_commit(tests.head_sha.clone()).is_ok();
    Some(if known {
        format!(
            "note: these tests were checked on {}; this checkout is at {}. Results can differ.",
            short(&tests.head_sha),
            short(head)
        )
    } else {
        format!(
            "note: these tests were checked on {}, which this checkout does not have (git fetch origin {} to get it). Running them on {}.",
            short(&tests.head_sha),
            clean_text(&pr.head_ref),
            short(head)
        )
    })
}

fn run_clean(options: &Options, local: &Checkout) -> Result<i32, AutterError> {
    let mut prs: Vec<u64> = match options.pr {
        Some(pr) => vec![pr],
        None => std::fs::read_dir(manifest_dir(&local.git_dir))
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        name.strip_prefix("pr-")?
                            .strip_suffix(".json")?
                            .parse()
                            .ok()
                    })
                    .collect()
            })
            .unwrap_or_default(),
    };
    prs.sort_unstable();
    let mut reports = Vec::new();
    for pr in prs {
        let mut manifest = load_manifest(&local.git_dir, pr);
        let report = clean_manifest(&local.root, &mut manifest, options.force);
        save_manifest(&local.git_dir, &manifest)?;
        reports.push((pr, report));
    }
    let kept = reports.iter().any(|(_, r)| !r.kept.is_empty());
    if arg_parser::json() {
        let out: Vec<Value> = reports
            .iter()
            .map(|(pr, r)| json!({ "pr": pr, "removed": r.removed, "kept": r.kept }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "cleaned": out }))?
        );
    } else if reports
        .iter()
        .all(|(_, r)| r.removed.is_empty() && r.kept.is_empty())
    {
        println!("Nothing to clean: no generated tests were pulled into this checkout.");
    } else {
        for (pr, r) in &reports {
            for path in &r.removed {
                println!("removed {path}  (#{pr})");
            }
            for path in &r.kept {
                println!("kept    {path}  (#{pr}): you edited it; --force removes it");
            }
        }
    }
    Ok(if kept { 1 } else { 0 })
}

fn run(options: Options) -> Result<i32, AutterError> {
    let config = Config::get();
    let local = std::env::current_dir()
        .ok()
        .and_then(|cwd| discover_repository_in_path_no_git_exec(&cwd).ok());
    if options.subcommand == "clean" {
        return run_clean(&options, &checkout(local)?);
    }
    let (repository, remotes) = target_repository(options.repository.as_deref(), local.as_ref())?;
    if let Some(reason) = sharing_refusal(
        config.platform_sync_enabled(),
        config.effective_prompt_storage_with_remotes(remotes.as_ref()),
    ) {
        return Err(AutterError::Generic(reason.into()));
    }
    // list works from anywhere with --repo and --pr; pull and run write into a checkout.
    let local = if options.subcommand == "list" {
        local.and_then(|repo| checkout(Some(repo)).ok())
    } else {
        Some(checkout(local)?)
    };
    let branch = if options.repository.is_some() {
        None
    } else {
        local.as_ref().and_then(|c| c.branch.clone())
    };
    let response = fetch(&options, &repository, branch.as_deref())?;
    let Some(tests) = response.generated_tests.clone() else {
        let message = response.message.clone().unwrap_or_else(|| {
            format!(
                "Autter has not written tests for #{} yet.",
                response.pull_request.number
            )
        });
        if options.subcommand == "list" {
            if arg_parser::json() {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "repository": response.repository,
                        "pullRequest": response.pull_request,
                        "generatedTests": Value::Null,
                        "message": message,
                    }))?
                );
            } else {
                println!("{}", clean_text(&message));
            }
            return Ok(0);
        }
        return Err(AutterError::Generic(message));
    };

    if options.subcommand == "list" {
        if arg_parser::json() {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "repository": response.repository,
                    "pullRequest": response.pull_request,
                    "generatedTests": tests,
                }))?
            );
        } else {
            print!("{}", render_list(&response, &tests));
        }
        return Ok(0);
    }

    let local = local.expect("pull and run have a checkout");
    let chosen = select(&tests.tests, options.all, &options.only);
    if chosen.is_empty() {
        let message = if !options.only.is_empty() {
            format!(
                "No generated test matches --only {}. See them with: autter tests list --pr {}",
                options.only.join(", "),
                response.pull_request.number
            )
        } else if tests.tests.is_empty() {
            format!(
                "Autter kept no tests for #{}.",
                response.pull_request.number
            )
        } else {
            format!(
                "None of the tests Autter wrote for #{} fails. Pass --all to {} the passing ones.",
                response.pull_request.number, options.subcommand
            )
        };
        return Err(AutterError::Generic(message));
    }
    let human = !arg_parser::json();
    if human {
        eprint!("{}", render_header(&response, &tests));
        if let Some(notice) = head_notice(&local, &tests, &response.pull_request) {
            eprintln!("{notice}");
        }
    }

    let python = python_program();
    let plans: Vec<Plan> = chosen
        .iter()
        .map(|t| {
            let argv = run_argv(t, python);
            let skip = if argv.is_none() {
                Some(format!(
                    "autter cannot build a command to run {} with {}",
                    t.path, t.framework
                ))
            } else {
                skip_reason(t, options.database_url.as_deref())
            };
            (*t, argv, skip)
        })
        .collect();

    if options.subcommand == "run" && options.dry_run {
        let out: Vec<Value> = plans
            .iter()
            .map(|(t, argv, skip)| {
                json!({
                    "path": t.path,
                    "dir": t.dir,
                    "command": argv.as_ref().map(|a| display_argv(a)),
                    "skip": skip,
                })
            })
            .collect();
        if human {
            for (t, argv, skip) in &plans {
                match (argv, skip) {
                    (_, Some(reason)) => println!("skip {}: {reason}", t.path),
                    (Some(argv), None) => println!(
                        "would write {} and run in {}: {}",
                        t.path,
                        if t.dir.is_empty() { "." } else { &t.dir },
                        display_argv(argv)
                    ),
                    (None, None) => {}
                }
            }
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "dryRun": out }))?
            );
        }
        return Ok(0);
    }

    if options.subcommand == "run" && !options.yes {
        let runnable = plans.iter().filter(|(_, _, s)| s.is_none()).count();
        if runnable > 0 {
            eprintln!(
                "\nAutter's model wrote these tests from this PR's code. They ran in Autter's sandbox; here they run on your machine, with your environment:"
            );
            for (t, argv, skip) in &plans {
                if let (Some(argv), None) = (argv, skip) {
                    eprintln!("  {}  →  {}", t.path, display_argv(argv));
                }
            }
            eprintln!(
                "Read them first with `autter tests pull` if you do not trust the PR's author."
            );
            let question = format!("Run {}?", plural(runnable, "test file", "test files"));
            if !crate::ui::is_interactive() {
                return Err(AutterError::Generic(
                    "Not a terminal, so nothing was run. Pass --yes to run without asking.".into(),
                ));
            }
            if !crate::ui::confirm::confirm(&question, false) {
                eprintln!("Nothing was run.");
                return Ok(1);
            }
        }
    }

    // Write the files.
    let mut manifest = load_manifest(&local.git_dir, response.pull_request.number);
    manifest.head_sha = tests.head_sha.clone();
    let mut results: Vec<Value> = Vec::new();
    let mut ok = true;
    let mut written: Vec<(&GeneratedTest, &Vec<String>)> = Vec::new();
    for (test, argv, skip) in &plans {
        // `run` writes only what it runs; `pull` writes every chosen file, so
        // a test that cannot run here can still be read.
        if options.subcommand == "run"
            && let Some(reason) = skip
        {
            if human {
                eprintln!("skipped: {}: {reason}", test.path);
            }
            ok = false;
            results.push(
                json!({ "path": test.path, "run": RunResult::Skipped { reason: reason.clone() } }),
            );
            continue;
        }
        let outcome = write_test(&local.root, test, &mut manifest, options.force);
        if let WriteOutcome::Refused { reason } = &outcome {
            ok = false;
            if human {
                eprintln!("not written: {}: {reason}", test.path);
            }
            results.push(json!({ "path": test.path, "write": outcome }));
            continue;
        }
        if options.subcommand == "run" {
            if let Some(argv) = argv {
                written.push((*test, argv));
            }
            continue;
        }
        if human {
            println!(
                "{} {}{}",
                if outcome == WriteOutcome::Written {
                    "wrote    "
                } else {
                    "unchanged"
                },
                test.path,
                skip.as_ref()
                    .map(|r| format!("  (won't run here: {r})"))
                    .unwrap_or_default()
            );
        }
        results.push(json!({
            "path": test.path,
            "write": outcome,
            "command": argv.as_ref().map(|a| display_argv(a)),
            "dir": test.dir,
            "skip": skip,
        }));
    }
    save_manifest(&local.git_dir, &manifest)?;

    if options.subcommand == "pull" {
        if human {
            println!(
                "\nRun one with `autter tests run --pr {} --only <path>`, and remove them with `autter tests clean`.",
                response.pull_request.number
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "pulled": results }))?
            );
        }
        return Ok(if ok { 0 } else { 1 });
    }

    // Run them.
    for (test, argv) in &written {
        for name in &test.env_names {
            if std::env::var_os(name).is_none() {
                eprintln!(
                    "warning: {} reads {name}, which is not set here (Autter's run had it from the repository's environment settings).",
                    test.path
                );
            }
        }
        if human {
            eprintln!(
                "\n▶ {}  ({}: {})",
                test.path,
                if test.dir.is_empty() { "." } else { &test.dir },
                display_argv(argv)
            );
        }
        let result = run_one(&local.root, test, argv, options.database_url.as_deref());
        if result != RunResult::Passed {
            ok = false;
        }
        results.push(json!({ "path": test.path, "run": result }));
    }
    if human {
        eprintln!();
        for r in &results {
            let path = r["path"].as_str().unwrap_or("");
            let line = match r["run"]["result"].as_str() {
                Some("passed") => format!("✓ {path} passed"),
                Some("failed") => format!("✗ {path} failed"),
                Some("skipped") => format!(
                    "- {path} skipped: {}",
                    r["run"]["reason"].as_str().unwrap_or("")
                ),
                Some("not_started") => {
                    format!("! {path}: {}", r["run"]["reason"].as_str().unwrap_or(""))
                }
                _ => format!(
                    "! {path} not written: {}",
                    r["write"]["reason"].as_str().unwrap_or("")
                ),
            };
            eprintln!("{line}");
        }
        eprintln!("\nThe files stay in your checkout; remove them with `autter tests clean`.");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "pullRequest": response.pull_request.number,
                "headSha": tests.head_sha,
                "results": results,
            }))?
        );
    }
    Ok(if ok { 0 } else { 1 })
}

pub fn handle(args: &[String]) {
    let parsed = arg_parser::pre_parse(args, ScanMode::Full, true).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        std::process::exit(2);
    });
    arg_parser::merge_global_flags(&parsed.flags);
    if parsed.flags.help {
        arg_parser::print_command_help("tests");
        return;
    }
    if let Some(directory) = parsed.flags.change_dir
        && let Err(error) = std::env::set_current_dir(directory)
    {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
    let options = parse_options(&parsed.rest).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        arg_parser::print_command_help("tests");
        std::process::exit(2);
    });
    match run(options) {
        Ok(code) => std::process::exit(code),
        Err(AutterError::Generic(message)) => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        Err(other) => {
            eprintln!("error: {other}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn test_file(path: &str, framework: &str, status: &str) -> GeneratedTest {
        GeneratedTest {
            gap_id: format!("{path}#unit"),
            path: path.into(),
            dir: String::new(),
            framework: framework.into(),
            command: "rm -rf / # never executed".into(),
            source: format!("// {path}\n"),
            cases: vec![],
            status: status.into(),
            behaviour: "existing".into(),
            uses_database: Some(false),
            database_env_names: vec![],
            env_names: vec![],
        }
    }

    #[test]
    fn options_take_a_subcommand_a_pr_and_filters() {
        let o = parse_options(&args(&[
            "run",
            "--pr",
            "42",
            "--only",
            "src/a.test.ts",
            "--only",
            "b#c",
            "--yes",
            "--database-url",
            "postgres://u:p@localhost/scratch",
        ]))
        .unwrap();
        assert_eq!(o.subcommand, "run");
        assert_eq!(o.pr, Some(42));
        assert_eq!(o.only, vec!["src/a.test.ts", "b#c"]);
        assert!(o.yes);
        assert_eq!(
            o.database_url.as_deref(),
            Some("postgres://u:p@localhost/scratch")
        );
        // A bare number (or #number) after the subcommand is the PR.
        assert_eq!(parse_options(&args(&["list", "#7"])).unwrap().pr, Some(7));
        assert_eq!(
            parse_options(&args(&["pull", "7", "--all"])).unwrap().pr,
            Some(7)
        );
    }

    #[test]
    fn options_are_validated_before_anything_is_sent() {
        assert!(parse_options(&args(&[])).is_err());
        assert!(parse_options(&args(&["fix"])).is_err());
        assert!(parse_options(&args(&["run", "--pr", "0"])).is_err());
        assert!(parse_options(&args(&["run", "--pr", "x"])).is_err());
        assert!(parse_options(&args(&["run", "--pr"])).is_err());
        assert!(parse_options(&args(&["pull", "--yes"])).is_err());
        assert!(parse_options(&args(&["list", "--dry-run"])).is_err());
        assert!(parse_options(&args(&["list", "--database-url", "postgres://x/y"])).is_err());
        assert!(parse_options(&args(&["run", "--database-url", "mysql://x/y"])).is_err());
        assert!(parse_options(&args(&["run", "--database-url", "not a url"])).is_err());
        assert!(parse_options(&args(&["clean", "--all"])).is_err());
        assert!(parse_options(&args(&["list", "--force"])).is_err());
        assert!(parse_options(&args(&["list", "1", "2"])).is_err());
        assert!(parse_options(&args(&["run", "--bogus", "1"])).is_err());
    }

    #[test]
    fn the_failing_tests_are_the_default_selection() {
        let tests = vec![
            test_file("a.test.js", "node-test", "failed"),
            test_file("b.test.js", "node-test", "passed"),
        ];
        let paths = |v: Vec<&GeneratedTest>| v.iter().map(|t| t.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(select(&tests, false, &[])), vec!["a.test.js"]);
        assert_eq!(
            paths(select(&tests, true, &[])),
            vec!["a.test.js", "b.test.js"]
        );
        assert_eq!(
            paths(select(&tests, false, &["b.test.js".into()])),
            vec!["b.test.js"]
        );
        assert_eq!(
            paths(select(&tests, false, &["a.test.js#unit".into()])),
            vec!["a.test.js"]
        );
    }

    #[test]
    fn only_paths_inside_the_tree_are_written() {
        assert!(safe_relative_path("src/a.test.ts").is_some());
        assert!(safe_relative_path("tests/x_test.py").is_some());
        for bad in [
            "",
            "/etc/passwd",
            "../x.test.js",
            "src/../../x.js",
            ".git/hooks/pre-commit",
            ".github/workflows/x.yml",
            "src\\a.js",
            "./",
        ] {
            assert!(safe_relative_path(bad).is_none(), "{bad} accepted");
        }
        assert_eq!(safe_dir(""), Some(PathBuf::new()));
        assert!(safe_dir("../web").is_none());
    }

    #[test]
    fn pull_never_overwrites_a_file_it_did_not_write_and_clean_removes_only_its_own() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/existing.test.js"), "mine").unwrap();
        let mut manifest = Manifest {
            pr: 3,
            ..Manifest::default()
        };

        let new = test_file("src/__tests__/deep/a.test.js", "node-test", "failed");
        assert_eq!(
            write_test(root, &new, &mut manifest, false),
            WriteOutcome::Written
        );
        assert_eq!(
            std::fs::read_to_string(root.join(&new.path)).unwrap(),
            new.source
        );
        assert_eq!(manifest.dirs, vec!["src/__tests__", "src/__tests__/deep"]);
        // Pulling again is a no-op.
        assert_eq!(
            write_test(root, &new, &mut manifest, false),
            WriteOutcome::Unchanged
        );

        let clash = test_file("src/existing.test.js", "node-test", "failed");
        assert!(matches!(
            write_test(root, &clash, &mut manifest, false),
            WriteOutcome::Refused { .. }
        ));
        assert!(
            matches!(
                write_test(root, &clash, &mut manifest, true),
                WriteOutcome::Refused { .. }
            ),
            "--force never replaces a file Autter did not write"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/existing.test.js")).unwrap(),
            "mine"
        );

        // A regenerated test replaces the copy pull wrote, unless it was edited.
        let mut regenerated = new.clone();
        regenerated.source = "// v2\n".into();
        assert_eq!(
            write_test(root, &regenerated, &mut manifest, false),
            WriteOutcome::Written
        );
        std::fs::write(root.join(&new.path), "// my debugging\n").unwrap();
        assert!(matches!(
            write_test(root, &new, &mut manifest, false),
            WriteOutcome::Refused { .. }
        ));

        // clean keeps the edited file, then --force removes it and the directories pull made.
        let other = test_file("src/b.test.js", "node-test", "failed");
        write_test(root, &other, &mut manifest, false);
        let report = clean_manifest(root, &mut manifest, false);
        assert_eq!(report.removed, vec!["src/b.test.js"]);
        assert_eq!(report.kept, vec!["src/__tests__/deep/a.test.js"]);
        assert!(root.join("src/__tests__/deep/a.test.js").exists());
        let report = clean_manifest(root, &mut manifest, true);
        assert_eq!(report.removed, vec!["src/__tests__/deep/a.test.js"]);
        assert!(!root.join("src/__tests__").exists());
        assert!(root.join("src/existing.test.js").exists());
        assert!(manifest.files.is_empty() && manifest.dirs.is_empty());
    }

    #[test]
    fn the_manifest_lives_in_the_git_dir_and_disappears_when_empty() {
        let git_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest {
            pr: 9,
            head_sha: "abc".into(),
            ..Manifest::default()
        };
        manifest.files.insert("a.test.js".into(), "h".into());
        save_manifest(git_dir.path(), &manifest).unwrap();
        assert_eq!(load_manifest(git_dir.path(), 9), manifest);
        manifest.files.clear();
        save_manifest(git_dir.path(), &manifest).unwrap();
        assert!(!manifest_path(git_dir.path(), 9).exists());
        assert_eq!(load_manifest(git_dir.path(), 9).files.len(), 0);
    }

    #[test]
    fn commands_are_built_from_the_framework_and_path_never_the_stored_string() {
        let argv = |framework: &str, path: &str, dir: &str| {
            let mut t = test_file(path, framework, "failed");
            t.dir = dir.into();
            run_argv(&t, "python3").map(|a| display_argv(&a))
        };
        assert_eq!(
            argv("vitest", "web/src/a.test.ts", "web").unwrap(),
            "npx vitest run --reporter=verbose src/a.test.ts"
        );
        assert_eq!(
            argv("jest", "src/a.test.js", "").unwrap(),
            "npx jest --ci --verbose --runTestsByPath src/a.test.js"
        );
        assert_eq!(
            argv("mocha", "test/a.spec.ts", "").unwrap(),
            "npx mocha --require tsx test/a.spec.ts"
        );
        assert_eq!(
            argv("node-test", "src/a.test.mjs", "").unwrap(),
            "node --test src/a.test.mjs"
        );
        assert_eq!(
            argv("node-test", "src/a.test.ts", "").unwrap(),
            "npx tsx --test src/a.test.ts"
        );
        assert_eq!(
            argv("pytest", "api/tests/test_x.py", "api").unwrap(),
            "python3 -m pytest -q -rf -p no:cacheprovider tests/test_x.py"
        );
        assert_eq!(
            argv("go-test", "svc/pkg/x_autter_test.go", "svc").unwrap(),
            "go test -count=1 -run '^TestAutter' ./pkg"
        );
        assert_eq!(
            argv("go-test", "x_autter_test.go", "").unwrap(),
            "go test -count=1 -run '^TestAutter' ."
        );
        assert_eq!(
            argv("cargo-test", "tests/lib_autter.rs", "").unwrap(),
            "cargo test --quiet --test lib_autter"
        );
        assert_eq!(
            argv("rspec", "spec/a_spec.rb", "").unwrap(),
            "bundle exec rspec spec/a_spec.rb"
        );
        assert_eq!(
            argv("phpunit", "tests/ATest.php", "").unwrap(),
            "vendor/bin/phpunit tests/ATest.php"
        );
        assert_eq!(argv("unknown", "a.test.js", ""), None);
        // A test outside its run directory is not run.
        assert_eq!(argv("jest", "src/a.test.js", "web"), None);
        // A name with shell metacharacters is not run (Windows runs through cmd /C).
        for odd in [
            "src/a; rm -rf ~.test.js",
            "src/a&calc.test.js",
            "src/$(id).test.js",
            "src/a|b.test.js",
        ] {
            assert_eq!(
                run_argv(&test_file(odd, "jest", "failed"), "python"),
                None,
                "{odd}"
            );
        }
        assert!(
            run_argv(
                &test_file("src/@scope/a b+c.test.js", "jest", "failed"),
                "python"
            )
            .is_some()
        );
    }

    #[test]
    fn a_database_test_runs_only_with_an_explicit_scratch_url() {
        let mut t = test_file("src/repo.test.js", "node-test", "failed");
        t.uses_database = Some(true);
        t.database_env_names = vec![
            "DATABASE_URL".into(),
            "PGHOST".into(),
            "POSTGRES_DB".into(),
            "DIRECT_URL".into(),
        ];
        assert!(skip_reason(&t, None).unwrap().contains("--database-url"));
        assert_eq!(
            skip_reason(&t, Some("postgres://u:p%40ss@db.local:6543/scratch")),
            None
        );
        let env: BTreeMap<String, String> =
            database_env(&t, "postgres://u:p%40ss@db.local:6543/scratch")
                .into_iter()
                .collect();
        assert_eq!(
            env["DATABASE_URL"],
            "postgres://u:p%40ss@db.local:6543/scratch"
        );
        assert_eq!(
            env["DIRECT_URL"],
            "postgres://u:p%40ss@db.local:6543/scratch"
        );
        assert_eq!(
            env["AUTTER_TEST_DATABASE_URL"],
            "postgres://u:p%40ss@db.local:6543/scratch"
        );
        assert_eq!(env["PGHOST"], "db.local");
        assert_eq!(env["POSTGRES_DB"], "scratch");

        // Results stored before the flag: the source decides.
        let mut old = test_file("src/old.test.js", "node-test", "failed");
        old.uses_database = None;
        assert_eq!(skip_reason(&old, None), None);
        old.source = "const db = postgres(process.env.DATABASE_URL)".into();
        assert!(skip_reason(&old, None).is_some());
        assert!(source_reads_database(
            "import { PrismaClient } from '@prisma/client'; new PrismaClient() // prisma"
        ));
        assert!(!source_reads_database("assert.equal(add(1, 2), 3)"));
    }

    #[test]
    fn the_list_names_failures_and_how_to_run_them() {
        let mut failing = test_file("src/a.test.js", "node-test", "failed");
        failing.uses_database = Some(true);
        let response = ApiResponse {
            repository: "acme/api".into(),
            pull_request: PullRequest {
                number: 12,
                title: Some("Discounts".into()),
                state: "open".into(),
                head_ref: "feat/discount".into(),
                head_sha: "bbbbbbbbbb".into(),
                html_url: None,
            },
            generated_tests: None,
            message: None,
        };
        let tests = GeneratedTests {
            head_sha: "aaaaaaaaaa".into(),
            base_sha: String::new(),
            is_stale: true,
            tests: vec![failing, test_file("src/b.test.js", "node-test", "passed")],
            failures: vec![Failure {
                gap_id: "src/a.test.js#unit".into(),
                test_path: "src/a.test.js".into(),
                test_name: "takes 10% off".into(),
                explanation: "The discount is added\u{1b}[31m instead of subtracted.".into(),
                location: "src/svc.js:3".into(),
                output: String::new(),
            }],
            could_not_run: vec![CouldNotRun {
                gap_id: None,
                reason: "pay: needs STRIPE_SECRET_KEY".into(),
                needs: vec![],
            }],
            dropped: vec![],
        };
        let out = render_list(&response, &tests);
        assert!(out.contains("acme/api #12 — Discounts (feat/discount)"));
        assert!(out.contains("1 passed, 1 found a bug"));
        assert!(out.contains("moved on to bbbbbbb"));
        assert!(out.contains("✗ src/a.test.js  [node-test]  (needs a database)"));
        assert!(out.contains("takes 10% off — The discount is added[31m instead of subtracted."));
        assert!(
            !out.contains('\u{1b}'),
            "no terminal escapes from the server"
        );
        assert!(out.contains("at src/svc.js:3"));
        assert!(out.contains("pay: needs STRIPE_SECRET_KEY"));
        assert!(out.contains("autter tests run --pr 12\n"));
    }
}
