//! `autter findings`: the review findings of the pull request open for the
//! current branch, placed on the lines of your working tree.
//!
//! Reads `GET /api/cli/orgs/<org>/findings?repository=<remote>&branch=<name>`
//! with the sign-in and organization choice of `autter ask`. Every finding's
//! line is a line of the file at the commit the review ran on
//! (`review.headSha`). The command diffs that commit against the working tree
//! (`git diff -U0 <headSha>`) and moves each line by the lines added and
//! removed above it. A finding whose own line changed since the review is
//! marked `changed` and given no line: the editor must never point at code the
//! review did not see.
//!
//! `--write` also writes `.autter/findings.md` (summary, location, next steps
//! and a link for each finding) and keeps `.autter/` out of git through
//! `.git/info/exclude`. `--json` prints the response with a `local` object on
//! every finding; the VS Code extension reads it.
use crate::api::client::ApiContext;
use crate::commands::arg_parser::{self, ScanMode};
use crate::commands::ask::{component, org_api_context};
use crate::error::AutterError;
use crate::git::repository::{Repository, exec_git, exec_git_allow_nonzero};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) const TIMEOUT_SECS: u64 = 30;
/// A missing reviewed commit is fetched from the PR's ref at most this long.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const FINDINGS_DIR: &str = ".autter";
pub(crate) const FINDINGS_FILE: &str = "findings.md";

const NOT_SIGNED_IN: &str = "Autter did not accept your sign-in for this organization (HTTP 401). Sign in with `autter login` first, or choose an organization with --org <slug>.";

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Options {
    branch: Option<String>,
    pr: Option<u64>,
    repository: Option<String>,
    organization: Option<String>,
    write: bool,
    no_fetch: bool,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--write" => {
                options.write = true;
                index += 1;
                continue;
            }
            "--no-fetch" => {
                options.no_fetch = true;
                index += 1;
                continue;
            }
            "--branch" | "--pr" | "--repo" | "--org" => {}
            _ => return Err(format!("unknown option for findings: {arg}")),
        }
        let value = args
            .get(index + 1)
            .map(|value| value.trim())
            .filter(|value| !value.starts_with("--") && !value.is_empty())
            .ok_or_else(|| format!("{arg} requires a value"))?;
        match arg {
            "--branch" => options.branch = Some(value.to_owned()),
            "--pr" => {
                let number = value.trim_start_matches('#').parse::<u64>().ok();
                options.pr = Some(
                    number
                        .filter(|n| *n > 0)
                        .ok_or("--pr takes a pull request number")?,
                );
            }
            "--repo" => options.repository = Some(value.to_owned()),
            _ => options.organization = Some(value.to_owned()),
        }
        index += 2;
    }
    Ok(options)
}

// ---------------------------------------------------------------------------
// Line mapping: reviewed commit → working tree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
struct Hunk {
    old_start: u32,
    old_count: u32,
    new_count: u32,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct FileDiff {
    deleted: bool,
    /// A binary file that differs: no line can be trusted.
    binary: bool,
    hunks: Vec<Hunk>,
}

/// Where a finding sits in the working tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Placement {
    /// Same line as in the review.
    Unchanged(u32),
    /// The line is untouched; lines above it were added or removed.
    Moved(u32),
    /// The line itself was edited or removed since the review.
    Changed,
    /// The file no longer exists.
    FileDeleted,
    /// The reviewed commit is not available locally, so the line can't be checked.
    Unverified,
    /// About the PR or a removed (base) line: no place in the working tree.
    NoLine,
}

impl Placement {
    fn status(self) -> &'static str {
        match self {
            Placement::Unchanged(_) => "unchanged",
            Placement::Moved(_) => "moved",
            Placement::Changed => "changed",
            Placement::FileDeleted => "file_deleted",
            Placement::Unverified => "unverified",
            Placement::NoLine => "no_line",
        }
    }

    fn line(self) -> Option<u32> {
        match self {
            Placement::Unchanged(line) | Placement::Moved(line) => Some(line),
            _ => None,
        }
    }
}

/// `-a,b` / `+c,d` of a hunk header; a missing count is 1.
fn parse_range(range: &str) -> Option<(u32, u32)> {
    let (start, count) = match range.split_once(',') {
        Some((start, count)) => (start, count.parse().ok()?),
        None => (range, 1),
    };
    Some((start.parse().ok()?, count))
}

fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let (old_start, old_count) = parse_range(old)?;
    let (_, new_count) = parse_range(new)?;
    Some(Hunk {
        old_start,
        old_count,
        new_count,
    })
}

/// A path from a `---`/`+++` header: strip the `a/`/`b/` prefix, the tab git
/// adds after names with spaces, and C quoting.
fn header_path(raw: &str, prefix: &str) -> Option<String> {
    let raw = raw.trim_end_matches('\t');
    if raw == "/dev/null" {
        return None;
    }
    let unquoted = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        unquote_c(&raw[1..raw.len() - 1])
    } else {
        raw.to_owned()
    };
    Some(
        unquoted
            .strip_prefix(prefix)
            .unwrap_or(&unquoted)
            .to_owned(),
    )
}

fn unquote_c(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut iter = s.bytes().peekable();
    while let Some(b) = iter.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match iter.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'"') => bytes.push(b'"'),
            Some(b'\\') => bytes.push(b'\\'),
            Some(d @ b'0'..=b'7') => {
                let mut value = (d - b'0') as u32;
                for _ in 0..2 {
                    match iter.peek() {
                        Some(n @ b'0'..=b'7') => {
                            value = value * 8 + (*n - b'0') as u32;
                            iter.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => {
                bytes.push(b'\\');
                bytes.push(other);
            }
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parse `git diff -U0 --no-renames <commit>` output into hunks per old path.
/// `---`/`+++` are read only in a file's header, so a removed line that
/// starts with `--` is never mistaken for one.
pub(crate) fn parse_diff(diff: &str) -> HashMap<String, FileDiff> {
    let mut files: HashMap<String, FileDiff> = HashMap::new();
    let mut current: Option<String> = None;
    let mut in_header = false;
    let mut pending_old: Option<String> = None;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_header = true;
            current = None;
            pending_old = None;
            continue;
        }
        if in_header {
            if let Some(raw) = line.strip_prefix("--- ") {
                pending_old = header_path(raw, "a/");
                continue;
            }
            if let Some(raw) = line.strip_prefix("+++ ") {
                if let Some(old) = pending_old.take() {
                    let entry = files.entry(old.clone()).or_default();
                    entry.deleted = header_path(raw, "b/").is_none();
                    current = Some(old);
                }
                continue;
            }
            if line.starts_with("Binary files ") {
                // `Binary files a/x and /dev/null differ` — the path is not
                // reliably parseable; mark it by the preceding `diff --git`.
                if let Some(path) = binary_path(line) {
                    let entry = files.entry(path).or_default();
                    entry.binary = true;
                    entry.deleted = line.ends_with(" and /dev/null differ");
                }
                continue;
            }
        }
        if line.starts_with("@@ ") {
            in_header = false;
            if let (Some(path), Some(hunk)) = (current.as_ref(), parse_hunk_header(line))
                && let Some(entry) = files.get_mut(path)
            {
                entry.hunks.push(hunk);
            }
        }
    }
    for file in files.values_mut() {
        file.hunks.sort_by_key(|h| h.old_start);
    }
    files
}

fn binary_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("Binary files ")?;
    let (old, _) = rest.split_once(" and ")?;
    header_path(old, "a/")
}

/// Where line `line` of the reviewed file is now, given that file's diff.
pub(crate) fn map_line(diff: Option<&FileDiff>, line: u32) -> Placement {
    let Some(diff) = diff else {
        return Placement::Unchanged(line);
    };
    if diff.deleted {
        return Placement::FileDeleted;
    }
    if diff.binary {
        return Placement::Changed;
    }
    let mut delta: i64 = 0;
    for hunk in &diff.hunks {
        if hunk.old_count == 0 {
            // Pure insertion after line `old_start`.
            if hunk.old_start < line {
                delta += hunk.new_count as i64;
                continue;
            }
            break;
        }
        let end = hunk.old_start + hunk.old_count - 1;
        if end < line {
            delta += hunk.new_count as i64 - hunk.old_count as i64;
        } else if hunk.old_start <= line {
            return Placement::Changed;
        } else {
            break;
        }
    }
    let moved = line as i64 + delta;
    if moved < 1 {
        return Placement::Changed;
    }
    if delta == 0 {
        Placement::Unchanged(line)
    } else {
        Placement::Moved(moved as u32)
    }
}

pub(crate) fn finding_has_line(finding: &Value) -> bool {
    finding["line"].as_u64().unwrap_or(0) > 0
        && finding["side"].as_str() != Some("LEFT")
        && !finding["prScoped"].as_bool().unwrap_or(false)
        && finding["path"].as_str().is_some_and(|p| !p.is_empty())
}

/// Place every finding (sets `finding.local = {status, line}`).
fn place_findings(findings: &mut [Value], diffs: Option<&HashMap<String, FileDiff>>) {
    for finding in findings.iter_mut() {
        let placement = if !finding_has_line(finding) {
            Placement::NoLine
        } else if let Some(diffs) = diffs {
            let path = finding["path"].as_str().unwrap_or_default();
            let line = finding["line"].as_u64().unwrap_or(0) as u32;
            map_line(diffs.get(path), line)
        } else {
            Placement::Unverified
        };
        finding["local"] = json!({ "status": placement.status(), "line": placement.line() });
    }
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

pub(crate) fn git_args(repo: &Repository, rest: &[&str]) -> Vec<String> {
    let mut args = repo.global_args_for_exec();
    args.extend(rest.iter().map(|s| (*s).to_owned()));
    args
}

fn has_commit(repo: &Repository, sha: &str) -> bool {
    let spec = format!("{sha}^{{commit}}");
    exec_git_allow_nonzero(&git_args(repo, &["cat-file", "-e", &spec]))
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Fetch `refs/pull/<n>/head` (GitHub keeps every pushed head there) so the
/// reviewed commit exists locally. Never prompts for credentials, gives up
/// after [`FETCH_TIMEOUT`], and writes no local ref (only FETCH_HEAD).
fn fetch_pull_head(repo: &Repository, pr: u64) -> bool {
    let Ok(Some(remote)) = repo.get_default_remote() else {
        return false;
    };
    let refspec = format!("refs/pull/{pr}/head");
    let args = git_args(
        repo,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            &remote,
            &refspec,
        ],
    );
    let mut command = std::process::Command::new(crate::config::Config::get().git_cmd());
    command
        .args(&args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < FETCH_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// The diff from `sha` to the working tree for `paths`.
fn working_tree_diff(
    repo: &Repository,
    sha: &str,
    paths: &BTreeSet<String>,
) -> Result<HashMap<String, FileDiff>, AutterError> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }
    let mut args = git_args(
        repo,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "-U0",
            "--no-color",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            sha,
            "--",
        ],
    );
    args.extend(paths.iter().map(|p| format!(":(literal){p}")));
    let output = exec_git(&args)?;
    Ok(parse_diff(&String::from_utf8_lossy(&output.stdout)))
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

fn findings_path(slug: &str, repository: &str, branch: Option<&str>, pr: Option<u64>) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("repository", repository);
    if let Some(branch) = branch {
        query.append_pair("branch", branch);
    }
    if let Some(pr) = pr {
        query.append_pair("pr", &pr.to_string());
    }
    format!(
        "/api/cli/orgs/{}/findings?{}",
        component(slug),
        query.finish()
    )
}

fn response_value(code: u16, body: &str) -> Result<Value, AutterError> {
    let value: Option<Value> = serde_json::from_str(body).ok();
    if code == 200
        && let Some(value) = value
    {
        return Ok(value);
    }
    if code == 401 {
        return Err(AutterError::Generic(NOT_SIGNED_IN.into()));
    }
    let message = value
        .as_ref()
        .and_then(|v| v["message"].as_str().or(v["error"].as_str()))
        .map(str::to_owned);
    Err(AutterError::Generic(match (code, message) {
        (404, Some(message)) if message.contains("Unknown") || message.contains("not found") => {
            message
        }
        (404, None) => "Findings API not found (HTTP 404). Update the API deployment if this command is unavailable.".into(),
        (_, message) => format!(
            "HTTP {code}: {}",
            message.as_deref().unwrap_or("Findings request failed")
        ),
    }))
}

/// Everything `run` needs from the local repository.
pub(crate) struct Local {
    pub(crate) repo: Repository,
    pub(crate) workdir: PathBuf,
    pub(crate) repository: String,
    pub(crate) branch: Option<String>,
}

/// The repository you are in, its remote (or `repository`) and branch (or `branch`).
pub(crate) fn local_context_for(
    repository: Option<String>,
    branch: Option<String>,
) -> Result<Local, AutterError> {
    let repo = crate::git::repository::find_repository(&Vec::<String>::new())
        .map_err(|_| AutterError::Generic("Run this inside a git repository.".into()))?;
    let workdir = repo.workdir()?;
    let repository = match repository {
        Some(repository) => repository,
        None => crate::authorship::post_commit::current_repo_url(&repo).ok_or_else(|| {
            AutterError::Generic("This repository has no remote. Pass --repo <owner/repo>.".into())
        })?,
    };
    let branch = match branch {
        Some(branch) => Some(branch),
        None => repo
            .head()
            .ok()
            .and_then(|head| head.shorthand().ok())
            .filter(|name| !name.is_empty() && name != "HEAD"),
    };
    Ok(Local {
        repo,
        workdir,
        repository,
        branch,
    })
}

fn local_context(options: &Options) -> Result<Local, AutterError> {
    let local = local_context_for(options.repository.clone(), options.branch.clone())?;
    if local.branch.is_none() && options.pr.is_none() {
        return Err(AutterError::Generic(
            "HEAD is detached. Check out a branch, or pass --branch <name> or --pr <number>."
                .into(),
        ));
    }
    Ok(local)
}

/// Fetch the findings and place them on the working tree.
fn collect(options: &Options, local: &Local) -> Result<Value, AutterError> {
    let (context, slug): (ApiContext, String) =
        org_api_context(options.organization.as_deref(), TIMEOUT_SECS)?;
    collect_with(&context, &slug, options, local)
}

/// [`collect`] with a signed-in context, for `autter review --write`.
pub(crate) fn collect_for_branch(
    context: &ApiContext,
    slug: &str,
    local: &Local,
) -> Result<Value, AutterError> {
    let options = Options {
        no_fetch: true,
        ..Options::default()
    };
    collect_with(context, slug, &options, local)
}

fn collect_with(
    context: &ApiContext,
    slug: &str,
    options: &Options,
    local: &Local,
) -> Result<Value, AutterError> {
    let branch = if options.pr.is_some() {
        None
    } else {
        local.branch.as_deref()
    };
    let mut result = if branch.is_none() && options.pr.is_none() {
        json!({ "pullRequest": null, "review": null, "findings": [] })
    } else {
        let path = findings_path(slug, &local.repository, branch, options.pr);
        let response = context.clone().with_timeout(TIMEOUT_SECS).get(&path)?;
        let body = response
            .as_str()
            .map_err(|error| AutterError::Generic(error.to_string()))?;
        response_value(response.status_code, body)?
    };

    let reviewed = result["review"]["headSha"].as_str().map(str::to_owned);
    let pr_number = result["pullRequest"]["number"].as_u64();
    let mut available = false;
    let mut diffs = None;
    if let Some(sha) = reviewed.as_deref() {
        available = has_commit(&local.repo, sha);
        if !available
            && !options.no_fetch
            && let Some(pr) = pr_number
        {
            available = fetch_pull_head(&local.repo, pr) && has_commit(&local.repo, sha);
        }
        if available {
            let paths: BTreeSet<String> = result["findings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|f| finding_has_line(f))
                .filter_map(|f| f["path"].as_str().map(str::to_owned))
                .collect();
            diffs = Some(working_tree_diff(&local.repo, sha, &paths)?);
        }
    }
    if let Some(findings) = result["findings"].as_array_mut() {
        place_findings(findings, diffs.as_ref());
    }
    result["local"] = json!({
        "branch": local.branch,
        "repository": local.repository,
        "workdir": local.workdir.to_string_lossy(),
        "reviewedCommitAvailable": available,
        "generatedAt": chrono::Utc::now().to_rfc3339(),
    });
    attach_local_review(&mut result, local);
    Ok(result)
}

// ---------------------------------------------------------------------------
// The saved local review (`autter review`)
// ---------------------------------------------------------------------------

pub(crate) const LOCAL_REVIEW_FILE: &str = "local-review.json";

pub(crate) fn local_review_path(local: &Local) -> PathBuf {
    local.workdir.join(FINDINGS_DIR).join(LOCAL_REVIEW_FILE)
}

/// Write each path's working-tree content into the object database and
/// return its blob id (`git hash-object -w --stdin-paths`). Missing files are
/// left out.
pub(crate) fn snapshot_blobs(
    local: &Local,
    paths: &[String],
) -> Result<HashMap<String, String>, AutterError> {
    use std::io::Write;
    let present: Vec<&String> = paths
        .iter()
        .filter(|p| local.workdir.join(p).is_file())
        .collect();
    if present.is_empty() {
        return Ok(HashMap::new());
    }
    let args = git_args(&local.repo, &["hash-object", "-w", "--stdin-paths"]);
    let mut child = std::process::Command::new(crate::config::Config::get().git_cmd())
        .args(&args)
        .current_dir(&local.workdir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| AutterError::Generic("Could not write to git hash-object.".into()))?;
        for path in &present {
            writeln!(stdin, "{path}")?;
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(AutterError::Generic(
            "git hash-object failed while saving the reviewed files.".into(),
        ));
    }
    let shas: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    Ok(present
        .into_iter()
        .cloned()
        .zip(shas)
        .collect::<HashMap<_, _>>())
}

/// Place a saved local review's findings on the working tree: each file is
/// compared with the copy the review saw (a blob saved at review time).
pub(crate) fn place_local_findings(local: &Local, saved: &Value) -> Vec<Value> {
    let mut findings: Vec<Value> = saved["result"]["findings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let reviewed: HashMap<String, String> = saved["files"]
        .as_object()
        .map(|files| {
            files
                .iter()
                .filter_map(|(path, sha)| sha.as_str().map(|sha| (path.clone(), sha.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let paths: Vec<String> = findings
        .iter()
        .filter(|f| finding_has_line(f))
        .filter_map(|f| f["path"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let current = snapshot_blobs(local, &paths).unwrap_or_default();
    // Diffs keyed by path: None when the file is gone, Some(empty) when unchanged.
    let mut diffs: HashMap<String, Option<FileDiff>> = HashMap::new();
    for path in &paths {
        let Some(before) = reviewed.get(path) else {
            continue;
        };
        let diff = match current.get(path) {
            None => None,
            Some(now) if now == before => Some(FileDiff::default()),
            Some(now) => {
                let args = git_args(
                    &local.repo,
                    &[
                        "diff",
                        "-U0",
                        "--no-color",
                        "--no-ext-diff",
                        "--no-textconv",
                        before,
                        now,
                    ],
                );
                match exec_git(&args) {
                    Ok(out) => parse_diff(&String::from_utf8_lossy(&out.stdout))
                        .remove(before.as_str())
                        .or(Some(FileDiff::default())),
                    Err(_) => continue,
                }
            }
        };
        diffs.insert(path.clone(), diff);
    }
    for finding in findings.iter_mut() {
        let placement = if !finding_has_line(finding) {
            Placement::NoLine
        } else {
            let path = finding["path"].as_str().unwrap_or_default();
            let line = finding["line"].as_u64().unwrap_or(0) as u32;
            match diffs.get(path) {
                Some(Some(diff)) => map_line(Some(diff), line),
                Some(None) => Placement::FileDeleted,
                None => Placement::Unverified,
            }
        };
        finding["local"] = json!({ "status": placement.status(), "line": placement.line() });
    }
    findings
}

/// Add `localReview` (the saved `autter review` result, placed on the working
/// tree) when there is one for the branch you are on.
pub(crate) fn attach_local_review(result: &mut Value, local: &Local) {
    let Ok(text) = std::fs::read_to_string(local_review_path(local)) else {
        return;
    };
    let Ok(saved) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    if saved["branch"].as_str() != local.branch.as_deref() {
        return;
    }
    let findings = place_local_findings(local, &saved);
    result["localReview"] = json!({
        "reviewId": saved["result"]["review"]["reviewId"],
        "reviewedAt": saved["reviewedAt"],
        "base": saved["base"],
        "baseRef": saved["baseRef"],
        "files": saved["files"].as_object().map(|f| f.len()).unwrap_or(0),
        "bundles": saved["result"]["review"]["bundles"],
        "findings": findings,
    });
}

// ---------------------------------------------------------------------------
// findings.md
// ---------------------------------------------------------------------------

/// Text without control characters (keeps newlines and tabs).
fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

pub(crate) fn one_line(text: &str) -> String {
    printable(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn severity_label(finding: &Value) -> &'static str {
    match finding["severity"].as_str() {
        Some("critical") => "Critical",
        Some("warning") => "Warning",
        _ => "Info",
    }
}

fn location(finding: &Value) -> String {
    let path = finding["path"].as_str().unwrap_or_default();
    match finding["local"]["line"].as_u64() {
        Some(line) => format!("{path}:{line}"),
        None if path.is_empty() => String::new(),
        None => path.to_owned(),
    }
}

/// `2026-11-17T09:12:34.000Z` → `2026-11-17 09:12 UTC`.
fn when(at: &str) -> String {
    format!("{} UTC", at.get(..16).unwrap_or(at).replace('T', " "))
}

/// Indent every line of a Markdown block so it stays inside a list item.
fn indented(text: &str) -> String {
    printable(text.trim())
        .lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("   {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_markdown_finding(out: &mut String, finding: &Value, number: usize) {
    let title = one_line(finding["title"].as_str().unwrap_or("Finding"));
    out.push_str(&format!(
        "{number}. **{}: {title}**\n",
        severity_label(finding)
    ));
    let loc = location(finding);
    if !loc.is_empty() {
        let note = match finding["local"]["status"].as_str() {
            Some("moved") => format!(
                " (line {} when reviewed)",
                finding["line"].as_u64().unwrap_or(0)
            ),
            Some("changed") => format!(
                ", line {} when reviewed; you have edited that line since",
                finding["line"].as_u64().unwrap_or(0)
            ),
            Some("file_deleted") => ", deleted since the review".into(),
            Some("unverified") => format!(
                ", line {} when reviewed; the reviewed copy isn't available locally to check it",
                finding["line"].as_u64().unwrap_or(0)
            ),
            _ => String::new(),
        };
        out.push_str(&format!("   - Location: `{loc}`{note}\n"));
    }
    let body = finding["body"].as_str().unwrap_or_default();
    if !body.trim().is_empty() {
        out.push_str("   - Summary:\n\n");
        out.push_str(&indented(body));
        out.push_str("\n\n");
    }
    if let Some(next) = finding["fixPrompt"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
    {
        out.push_str("   - Next steps:\n\n");
        out.push_str(&indented(next));
        out.push_str("\n\n");
    }
    if let Some(url) = finding["url"].as_str() {
        out.push_str(&format!("   - Link: {url}\n"));
    }
    // One blank line between items.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out.push('\n');
}

fn status_of(finding: &Value) -> &str {
    finding["local"]["status"].as_str().unwrap_or_default()
}

/// The counts line and the findings grouped into in-your-code, changed since
/// the review, and about the change as a whole.
fn render_markdown_groups(out: &mut String, findings: &[Value], number: &mut usize, whole: &str) {
    if findings.is_empty() {
        out.push_str("No findings.\n\n");
        return;
    }
    let pick = |statuses: &[&str]| -> Vec<&Value> {
        findings
            .iter()
            .filter(|f| statuses.contains(&status_of(f)))
            .collect()
    };
    let in_code = pick(&["unchanged", "moved", "unverified"]);
    let changed = pick(&["changed", "file_deleted"]);
    let general = pick(&["no_line"]);
    let critical = findings
        .iter()
        .filter(|f| f["severity"].as_str() == Some("critical"))
        .count();
    out.push_str(&format!(
        "**{} finding{}** ({critical} critical): {} in your code, {} on lines you have changed since the review, {} about the {whole}.\n\n",
        findings.len(),
        if findings.len() == 1 { "" } else { "s" },
        in_code.len(),
        changed.len(),
        general.len(),
    ));
    let about = format!("About the {whole}");
    for (heading, list) in [
        ("In your code", &in_code),
        ("Changed since the review (may be fixed)", &changed),
        (about.as_str(), &general),
    ] {
        if list.is_empty() {
            continue;
        }
        out.push_str(&format!("### {heading}\n\n"));
        for finding in list.iter() {
            *number += 1;
            render_markdown_finding(out, finding, *number);
        }
    }
}

fn findings_of(value: &Value) -> Vec<Value> {
    value["findings"].as_array().cloned().unwrap_or_default()
}

pub(crate) fn render_markdown(result: &Value) -> String {
    let mut out = String::from("# Autter findings\n\n");
    out.push_str("<!-- Written by `autter findings --write` and `autter review --write`. It is replaced on every run; don't edit it. -->\n\n");
    // No time of the run in here: the file only changes when the findings
    // do, so an editor showing it doesn't reload on every refresh.
    let branch = result["local"]["branch"].as_str().unwrap_or("this branch");
    let mut number = 0;

    out.push_str("## Pull request review\n\n");
    let pr = &result["pullRequest"];
    if pr.is_null() {
        out.push_str(&format!(
            "No open pull request for `{}` yet. Run `autter review` to review your changes before you open one.\n\n",
            one_line(branch)
        ));
    } else {
        let pr_number = pr["number"].as_u64().unwrap_or(0);
        let title = one_line(pr["title"].as_str().unwrap_or_default());
        match pr["url"].as_str() {
            Some(url) => out.push_str(&format!("Pull request [#{pr_number} {title}]({url})")),
            None => out.push_str(&format!("Pull request #{pr_number} {title}")),
        }
        out.push_str(&format!(" on `{}`.\n\n", one_line(branch)));
        let review = &result["review"];
        if review.is_null() {
            out.push_str("Autter has not reviewed this pull request yet.\n\n");
        } else {
            let sha = review["headSha"].as_str().unwrap_or_default();
            out.push_str(&format!("Reviewed commit `{}`", short_sha(sha)));
            if let Some(at) = review["reviewedAt"].as_str() {
                out.push_str(&format!(" at {}", when(at)));
            }
            if review["stale"].as_bool().unwrap_or(false) {
                out.push_str(" (an older push: the newest one has not been reviewed yet)");
            }
            out.push_str(".\n\n");
            render_markdown_groups(&mut out, &findings_of(result), &mut number, "pull request");
        }
    }

    let local_review = &result["localReview"];
    if !local_review.is_null() {
        out.push_str("## Local review\n\n");
        out.push_str("Your changes as `autter review` saw them");
        let base = match (
            local_review["baseRef"].as_str(),
            local_review["base"].as_str(),
        ) {
            (Some(name), _) if name != "HEAD" => Some(one_line(name)),
            (_, Some(sha)) => Some(short_sha(sha).to_owned()),
            _ => None,
        };
        if let Some(base) = base {
            out.push_str(&format!(", compared with `{base}`"));
        }
        if let Some(at) = local_review["reviewedAt"].as_str() {
            out.push_str(&format!(", at {}", when(at)));
        }
        out.push_str(". A local review runs the deterministic (rule-based) checks before a pull request exists, with no AI; the full review runs on the pull request.\n\n");
        render_markdown_groups(
            &mut out,
            &findings_of(local_review),
            &mut number,
            "change as a whole",
        );
    }
    out
}

/// `exclude` with `/.autter/` added, or `None` when it is already there.
fn with_exclude_entry(existing: &str) -> Option<String> {
    let entry = format!("/{FINDINGS_DIR}/");
    if existing.lines().any(|line| line.trim() == entry) {
        return None;
    }
    let mut text = existing.to_owned();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "# autter findings (autter findings --write)\n{entry}\n"
    ));
    Some(text)
}

/// Add `/.autter/` to `.git/info/exclude` (once) so the file never gets committed.
pub(crate) fn exclude_findings_dir(repo: &Repository) -> std::io::Result<()> {
    let info = repo.common_dir().join("info");
    let exclude = info.join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let Some(text) = with_exclude_entry(&existing) else {
        return Ok(());
    };
    std::fs::create_dir_all(&info)?;
    std::fs::write(exclude, text)
}

pub(crate) fn write_markdown(local: &Local, markdown: &str) -> Result<PathBuf, AutterError> {
    let dir = local.workdir.join(FINDINGS_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(FINDINGS_FILE);
    write_if_changed(&path, markdown)?;
    // Best effort: the file is still useful when info/exclude can't be written.
    let _ = exclude_findings_dir(&local.repo);
    Ok(path)
}

/// Rewrite only when the text changed, so editors watching it don't reload for nothing.
pub(crate) fn write_if_changed(path: &Path, text: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(());
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

// ---------------------------------------------------------------------------
// Terminal output
// ---------------------------------------------------------------------------

/// One line per finding with a place in the code (or about the change), then
/// how many are on lines changed since the review.
pub(crate) fn render_text_findings(out: &mut String, findings: &[Value]) {
    if findings.is_empty() {
        out.push_str("No findings.\n");
        return;
    }
    let mut changed = 0;
    for finding in findings {
        if matches!(status_of(finding), "changed" | "file_deleted") {
            changed += 1;
            continue;
        }
        let loc = location(finding);
        out.push_str(&format!(
            "{:<8}  {}{}\n",
            severity_label(finding).to_lowercase(),
            if loc.is_empty() {
                String::new()
            } else {
                format!("{loc}  ")
            },
            one_line(finding["title"].as_str().unwrap_or_default())
        ));
    }
    if changed > 0 {
        out.push_str(&format!(
            "\n{changed} finding{} on lines you have changed since the review (see --write or --json).\n",
            if changed == 1 { " is" } else { "s are" }
        ));
    }
}

fn render_text(result: &Value) -> String {
    let mut out = String::new();
    let pr = &result["pullRequest"];
    let branch = result["local"]["branch"].as_str().unwrap_or("this branch");
    if pr.is_null() {
        out.push_str(&format!(
            "No open pull request for {} yet. Run `autter review` to review your changes before you open one.\n",
            one_line(branch)
        ));
    } else {
        out.push_str(&format!(
            "PR #{} {}\n",
            pr["number"].as_u64().unwrap_or(0),
            one_line(pr["title"].as_str().unwrap_or_default())
        ));
        if let Some(url) = pr["url"].as_str() {
            out.push_str(&format!("{url}\n"));
        }
        let review = &result["review"];
        if review.is_null() {
            out.push_str("Autter has not reviewed this pull request yet.\n");
        } else {
            out.push_str(&format!(
                "Reviewed {}{}\n\n",
                short_sha(review["headSha"].as_str().unwrap_or_default()),
                if review["stale"].as_bool().unwrap_or(false) {
                    " (an older push; the newest is not reviewed yet)"
                } else {
                    ""
                }
            ));
            render_text_findings(&mut out, &findings_of(result));
            if result["local"]["reviewedCommitAvailable"].as_bool() == Some(false) {
                out.push_str(
                    "\nThe reviewed commit isn't available locally, so lines may have moved. Run `git fetch` and try again.\n",
                );
            }
        }
    }
    let local_review = &result["localReview"];
    if !local_review.is_null() {
        out.push_str(&format!(
            "\nLocal review{}\n\n",
            local_review["reviewedAt"]
                .as_str()
                .map(|at| format!(" ({})", when(at)))
                .unwrap_or_default()
        ));
        render_text_findings(&mut out, &findings_of(local_review));
    }
    out
}

fn run(options: Options) -> Result<(), AutterError> {
    let local = local_context(&options)?;
    let mut result = collect(&options, &local)?;
    if options.write {
        let path = write_markdown(&local, &render_markdown(&result))?;
        result["local"]["markdownPath"] = json!(path.to_string_lossy());
    }
    if arg_parser::json() {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print!("{}", render_text(&result));
        if let Some(path) = result["local"]["markdownPath"].as_str() {
            println!("\nWrote {path}");
        }
    }
    Ok(())
}

pub fn handle(args: &[String]) {
    let parsed = arg_parser::pre_parse(args, ScanMode::Full, true).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        std::process::exit(2);
    });
    arg_parser::merge_global_flags(&parsed.flags);
    if parsed.flags.help {
        arg_parser::print_command_help("findings");
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
        std::process::exit(2);
    });
    if let Err(error) = run(options) {
        let message = match error {
            AutterError::Generic(message) => message,
            other => other.to_string(),
        };
        if arg_parser::json() {
            println!("{}", json!({ "error": message }));
        } else {
            eprintln!("error: {message}");
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn parses_options() {
        assert_eq!(
            parse_options(&s(&[
                "--write",
                "--pr",
                "#12",
                "--org",
                "acme",
                "--no-fetch"
            ]))
            .unwrap(),
            Options {
                pr: Some(12),
                organization: Some("acme".into()),
                write: true,
                no_fetch: true,
                ..Options::default()
            }
        );
        assert!(parse_options(&s(&["--pr", "x"])).is_err());
        assert!(parse_options(&s(&["--branch"])).is_err());
        assert!(parse_options(&s(&["stray"])).is_err());
    }

    #[test]
    fn builds_the_query() {
        assert_eq!(
            findings_path("acme co", "https://github.com/a/b", Some("feat/x"), None),
            "/api/cli/orgs/acme+co/findings?repository=https%3A%2F%2Fgithub.com%2Fa%2Fb&branch=feat%2Fx"
        );
    }

    const DIFF: &str = "\
diff --git a/src/a.ts b/src/a.ts
index 111..222 100644
--- a/src/a.ts
+++ b/src/a.ts
@@ -2,0 +3,2 @@ fn top
+added one
+added two
@@ -10 +12 @@ fn mid
-old ten
+new ten
@@ -20,3 +22,0 @@
--- a removed SQL comment line
-x
-y
diff --git a/gone.ts b/gone.ts
deleted file mode 100644
--- a/gone.ts
+++ /dev/null
@@ -1,3 +0,0 @@
-a
-b
-c
diff --git \"a/sp\\303\\251cial \\\"q\\\".ts\" \"b/sp\\303\\251cial \\\"q\\\".ts\"
--- \"a/sp\\303\\251cial \\\"q\\\".ts\"
+++ \"b/sp\\303\\251cial \\\"q\\\".ts\"
@@ -1 +1 @@
-a
+b
";

    #[test]
    fn parses_hunks_per_file() {
        let files = parse_diff(DIFF);
        let a = &files["src/a.ts"];
        assert!(!a.deleted);
        assert_eq!(a.hunks.len(), 3);
        assert_eq!(
            a.hunks[2],
            Hunk {
                old_start: 20,
                old_count: 3,
                new_count: 0
            }
        );
        assert!(files["gone.ts"].deleted);
        assert!(files.contains_key("spécial \"q\".ts"));
        // The removed `-- a removed…` line must not open a new file.
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn maps_lines_through_the_diff() {
        let files = parse_diff(DIFF);
        let a = files.get("src/a.ts");
        assert_eq!(map_line(a, 1), Placement::Unchanged(1));
        assert_eq!(map_line(a, 2), Placement::Unchanged(2));
        // Two lines inserted after line 2.
        assert_eq!(map_line(a, 3), Placement::Moved(5));
        assert_eq!(map_line(a, 9), Placement::Moved(11));
        // Line 10 itself was edited.
        assert_eq!(map_line(a, 10), Placement::Changed);
        assert_eq!(map_line(a, 11), Placement::Moved(13));
        assert_eq!(map_line(a, 21), Placement::Changed);
        // Three lines removed above.
        assert_eq!(map_line(a, 30), Placement::Moved(29));
        assert_eq!(map_line(files.get("gone.ts"), 2), Placement::FileDeleted);
        assert_eq!(map_line(None, 7), Placement::Unchanged(7));
    }

    #[test]
    fn insertion_at_the_finding_line_does_not_move_it() {
        // Inserted after line 5: line 5 stays, line 6 moves.
        let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -5,0 +6 @@\n+new\n";
        let files = parse_diff(diff);
        assert_eq!(map_line(files.get("f"), 5), Placement::Unchanged(5));
        assert_eq!(map_line(files.get("f"), 6), Placement::Moved(7));
        // Inserted at the top of the file.
        let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -0,0 +1 @@\n+new\n";
        let files = parse_diff(diff);
        assert_eq!(map_line(files.get("f"), 1), Placement::Moved(2));
    }

    fn finding(path: &str, line: u64) -> Value {
        json!({ "id": "f", "title": "T", "body": "B", "path": path, "line": line, "side": "RIGHT",
                "severity": "critical", "prScoped": false, "fixPrompt": "Do X.", "url": "https://x/pull/1" })
    }

    #[test]
    fn places_findings_or_marks_them_unverified() {
        let files = parse_diff(DIFF);
        let mut list = vec![
            finding("src/a.ts", 10),
            finding("src/a.ts", 3),
            json!({ "path": "", "line": 0, "prScoped": true }),
        ];
        place_findings(&mut list, Some(&files));
        assert_eq!(
            list[0]["local"],
            json!({ "status": "changed", "line": null })
        );
        assert_eq!(list[1]["local"], json!({ "status": "moved", "line": 5 }));
        assert_eq!(list[2]["local"]["status"], "no_line");

        let mut list = vec![finding("src/a.ts", 3)];
        place_findings(&mut list, None);
        assert_eq!(
            list[0]["local"],
            json!({ "status": "unverified", "line": null })
        );
    }

    #[test]
    fn renders_markdown_sections() {
        let files = parse_diff(DIFF);
        let mut findings = vec![finding("src/a.ts", 3), finding("src/a.ts", 10)];
        place_findings(&mut findings, Some(&files));
        let result = json!({
            "pullRequest": { "number": 7, "title": "Add thing", "url": "https://app/pull/7" },
            "review": { "headSha": "abcdef123456", "stale": false, "reviewedAt": "2026-11-17T09:12:34.000Z" },
            "findings": findings,
            "local": { "branch": "feat", "generatedAt": "2026-11-17T00:00:00Z" },
        });
        let md = render_markdown(&result);
        assert!(md.contains("[#7 Add thing](https://app/pull/7)"));
        assert!(md.contains("Reviewed commit `abcdef1` at 2026-11-17 09:12 UTC."));
        assert!(!md.contains("2026-11-17T00:00:00Z"), "no run time: {md}");
        assert!(md.contains("## Pull request review"));
        assert!(md.contains("### In your code"));
        assert!(md.contains("Location: `src/a.ts:5` (line 3 when reviewed)"));
        assert!(md.contains("### Changed since the review"));
        assert!(!md.contains("## Local review"));
        assert!(md.contains("Location: `src/a.ts`, line 10 when reviewed"));
        assert!(md.contains("   - Next steps:\n\n   Do X."));

        let none = render_markdown(&json!({ "pullRequest": null, "local": { "branch": "feat" } }));
        assert!(none.contains("No open pull request for `feat` yet"));
    }

    #[test]
    fn excludes_the_findings_dir_once() {
        let once = with_exclude_entry("*.log").unwrap();
        assert_eq!(
            once,
            "*.log\n# autter findings (autter findings --write)\n/.autter/\n"
        );
        assert_eq!(with_exclude_entry(&once), None);
        assert!(with_exclude_entry("").unwrap().ends_with("/.autter/\n"));
    }

    /// The parser against what git really prints, not a hand-written diff.
    #[test]
    fn maps_lines_through_a_real_git_diff() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        git(&["init", "-q"]);
        let original: String = (1..=12).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.path().join("a b.ts"), &original).unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "reviewed"]);
        let reviewed = git(&["rev-parse", "HEAD"]).trim().to_owned();

        // Two lines added at the top, line 5 edited, line 9 removed.
        let edited = original
            .replacen("line 1\n", "new a\nnew b\nline 1\n", 1)
            .replace("line 5\n", "line five\n")
            .replace("line 9\n", "");
        std::fs::write(dir.path().join("a b.ts"), edited).unwrap();

        let diff = git(&[
            "-c",
            "core.quotePath=false",
            "diff",
            "-U0",
            "--no-color",
            "--no-renames",
            "--no-ext-diff",
            &reviewed,
            "--",
            ":(literal)a b.ts",
        ]);
        let files = parse_diff(&diff);
        let file = files.get("a b.ts");
        assert!(file.is_some(), "{diff}");
        assert_eq!(map_line(file, 1), Placement::Moved(3));
        assert_eq!(map_line(file, 4), Placement::Moved(6));
        assert_eq!(map_line(file, 5), Placement::Changed);
        assert_eq!(map_line(file, 8), Placement::Moved(10));
        assert_eq!(map_line(file, 9), Placement::Changed);
        assert_eq!(map_line(file, 10), Placement::Moved(11));
        assert_eq!(map_line(file, 12), Placement::Moved(13));
    }

    /// A saved local review is placed by diffing the reviewed blob against the file now.
    #[test]
    fn places_a_saved_local_review_on_the_working_tree() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        let original: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.path().join("a.ts"), &original).unwrap();
        std::fs::write(dir.path().join("b.ts"), "x\n").unwrap();

        let repo =
            crate::git::repository::find_repository_in_path(&dir.path().to_string_lossy()).unwrap();
        let local = Local {
            workdir: repo.workdir().unwrap(),
            repo,
            repository: "acme/web".into(),
            branch: Some("feat".into()),
        };
        let blobs = snapshot_blobs(&local, &["a.ts".into(), "b.ts".into()]).unwrap();
        assert_eq!(blobs.len(), 2);

        // After the review: a line added at the top, line 6 edited, b.ts deleted.
        let edited = format!("new\n{}", original.replace("line 6\n", "line six\n"));
        std::fs::write(dir.path().join("a.ts"), edited).unwrap();
        std::fs::remove_file(dir.path().join("b.ts")).unwrap();

        let saved = json!({
            "branch": "feat",
            "files": blobs,
            "result": { "findings": [
                finding("a.ts", 2), finding("a.ts", 6), finding("b.ts", 1), finding("c.ts", 1),
            ] },
        });
        let placed = place_local_findings(&local, &saved);
        let local_of = |i: usize| placed[i]["local"].clone();
        assert_eq!(local_of(0), json!({ "status": "moved", "line": 3 }));
        assert_eq!(local_of(1), json!({ "status": "changed", "line": null }));
        assert_eq!(
            local_of(2),
            json!({ "status": "file_deleted", "line": null })
        );
        // Not in the snapshot: can't be checked.
        assert_eq!(local_of(3), json!({ "status": "unverified", "line": null }));

        // Shown only on the branch it was made on.
        std::fs::create_dir_all(dir.path().join(FINDINGS_DIR)).unwrap();
        std::fs::write(local_review_path(&local), saved.to_string()).unwrap();
        let mut result = json!({});
        attach_local_review(&mut result, &local);
        assert_eq!(
            result["localReview"]["findings"].as_array().unwrap().len(),
            4
        );
        let other = Local {
            branch: Some("main".into()),
            ..local
        };
        let mut result = json!({});
        attach_local_review(&mut result, &other);
        assert!(result["localReview"].is_null());
    }
}
