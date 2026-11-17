//! `autter review`: review your changes before you open a pull request.
//!
//! Sends what a pull request from this branch would contain — the diff from
//! the merge base with the default branch to your working tree, untracked
//! files included — to `POST /api/cli/orgs/<org>/review`, which runs the
//! deterministic layer of the PR review's check bundles with no AI model
//! calls, and streams back JSON lines. The full review, AI checks included,
//! runs on the pull request, whose page also lists this review's findings.
//! `--dismiss <finding-id>` records a dismissal there (and hides it here).
//!
//! Before sending, every reviewed file is saved as a git blob. The result and
//! those blob ids go to `.autter/local-review.json`; `autter findings` (and so
//! the VS Code extension) reads it and places each finding on your working
//! tree by diffing the reviewed blob against the file as it is now, the same
//! way PR findings follow your edits. `--write` also writes
//! `.autter/findings.md` with the pull request's findings and this review.
use crate::commands::arg_parser::{self, ScanMode};
use crate::commands::ask::{component, org_api_context};
use crate::commands::findings::{
    self, FINDINGS_DIR, Local, git_args, local_context_for, local_review_path, one_line,
    place_local_findings, render_markdown, render_text_findings, short_sha, snapshot_blobs,
    write_if_changed, write_markdown,
};
use crate::error::AutterError;
use crate::git::repository::{exec_git, exec_git_allow_nonzero};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

/// A large change can take a while; the server streams while it runs.
const TIMEOUT_SECS: u64 = 600;
/// What the server takes (`LOCAL_REVIEW_MAX_FILES` / `_MAX_BYTES` in the backend).
const MAX_FILES: usize = 60;
const MAX_BYTES: usize = 3 * 1024 * 1024;
/// Files larger than this are sent as a patch only, without their content.
const MAX_CONTENT_BYTES: usize = 200 * 1024;

const NOT_SIGNED_IN: &str = "Autter did not accept your sign-in for this organization (HTTP 401). Sign in with `autter login` first, or choose an organization with --org <slug>.";

#[derive(Debug, Default, PartialEq)]
struct Options {
    paths: Vec<String>,
    base: Option<String>,
    uncommitted: bool,
    repository: Option<String>,
    organization: Option<String>,
    write: bool,
    /// `--dismiss <finding-id>`: record a dismissal instead of reviewing.
    dismiss: Option<String>,
    /// With `--dismiss`: clear the dismissal instead.
    undo: bool,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            options
                .paths
                .extend(args[index + 1..].iter().map(|p| p.trim().to_owned()));
            break;
        }
        match arg {
            "--write" => options.write = true,
            "--uncommitted" => options.uncommitted = true,
            "--undo" => options.undo = true,
            "--base" | "--repo" | "--org" | "--dismiss" => {
                let value = args
                    .get(index + 1)
                    .map(|v| v.trim())
                    .filter(|v| !v.starts_with("--") && !v.is_empty())
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                match arg {
                    "--base" => options.base = Some(value.to_owned()),
                    "--dismiss" => options.dismiss = Some(value.to_owned()),
                    "--repo" => options.repository = Some(value.to_owned()),
                    _ => options.organization = Some(value.to_owned()),
                }
                index += 2;
                continue;
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option for review: {arg}")),
            _ => options.paths.push(arg.trim().to_owned()),
        }
        index += 1;
    }
    if options.uncommitted && options.base.is_some() {
        return Err("--uncommitted and --base can't be used together".into());
    }
    if options.undo && options.dismiss.is_none() {
        return Err("--undo goes with --dismiss <finding-id>".into());
    }
    options.paths.retain(|p| !p.is_empty());
    Ok(options)
}

// ---------------------------------------------------------------------------
// What to review
// ---------------------------------------------------------------------------

fn git_text(local: &Local, rest: &[&str]) -> Result<String, AutterError> {
    let out = exec_git(&git_args(&local.repo, rest))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn rev_parse(local: &Local, spec: &str) -> Option<String> {
    let spec = format!("{spec}^{{commit}}");
    let out = exec_git_allow_nonzero(&git_args(
        &local.repo,
        &["rev-parse", "--verify", "--quiet", &spec],
    ))
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// The commit to compare with and how to name it: the merge base of HEAD
/// with `--base` or the default branch (what a PR would be opened against),
/// or HEAD itself with `--uncommitted` or when there is no default branch.
fn resolve_base(local: &Local, options: &Options) -> Result<(String, String), AutterError> {
    let head = rev_parse(local, "HEAD").ok_or_else(|| {
        AutterError::Generic(
            "This repository has no commits yet. Commit once, then run autter review.".into(),
        )
    })?;
    if options.uncommitted {
        return Ok((head, "HEAD".into()));
    }
    let candidates: Vec<String> = match options.base.clone() {
        Some(base) => vec![base],
        None => {
            let mut list = Vec::new();
            if let Ok(Some(remote)) = local.repo.get_default_remote() {
                if let Ok(head_ref) = local.repo.remote_head(&remote)
                    && !head_ref.is_empty()
                {
                    list.push(head_ref);
                }
                list.push(format!("{remote}/main"));
                list.push(format!("{remote}/master"));
            }
            list.push("main".into());
            list.push("master".into());
            list
        }
    };
    for candidate in &candidates {
        let Some(sha) = rev_parse(local, candidate) else {
            continue;
        };
        if let Ok(Some(base)) = local.repo.merge_base(head.clone(), sha) {
            return Ok((base, candidate.clone()));
        }
    }
    if let Some(base) = options.base.as_deref() {
        return Err(AutterError::Generic(format!(
            "Could not find a common commit with {base}. Pass another --base, or --uncommitted."
        )));
    }
    Ok((head, "HEAD".into()))
}

#[derive(Debug, Clone, PartialEq)]
struct Change {
    path: String,
    status: &'static str,
}

/// `git diff --name-status -z` output → changes.
fn parse_name_status(text: &str) -> Vec<Change> {
    let mut out = Vec::new();
    let mut parts = text.split('\0').filter(|s| !s.is_empty());
    while let Some(code) = parts.next() {
        let Some(path) = parts.next() else {
            break;
        };
        let status = match code.chars().next() {
            Some('A') => "added",
            Some('D') => "removed",
            _ => "modified",
        };
        out.push(Change {
            path: path.to_owned(),
            status,
        });
    }
    out
}

fn wanted(path: &str, filters: &[String]) -> bool {
    if path == FINDINGS_DIR || path.starts_with(&format!("{FINDINGS_DIR}/")) {
        return false;
    }
    filters.is_empty()
        || filters.iter().any(|f| {
            let f = f.trim_end_matches('/');
            f == "." || path == f || path.starts_with(&format!("{f}/"))
        })
}

/// Paths given on the command line, relative to the repository root.
fn repo_relative_filters(local: &Local, paths: &[String]) -> Vec<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| local.workdir.clone());
    paths
        .iter()
        .map(|p| {
            let abs = cwd.join(p);
            let abs = abs.canonicalize().unwrap_or(abs);
            let root = local
                .workdir
                .canonicalize()
                .unwrap_or_else(|_| local.workdir.clone());
            abs.strip_prefix(&root)
                .map(|rel| crate::utils::normalize_to_posix(&rel.to_string_lossy()))
                .unwrap_or_else(|_| p.clone())
        })
        .map(|p| if p.is_empty() { ".".into() } else { p })
        .collect()
}

fn changed_files(
    local: &Local,
    base: &str,
    filters: &[String],
) -> Result<Vec<Change>, AutterError> {
    let tracked = git_text(
        local,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--name-status",
            "--no-renames",
            "-z",
            base,
        ],
    )?;
    let mut changes = parse_name_status(&tracked);
    let untracked = git_text(local, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    changes.extend(
        untracked
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(|path| Change {
                path: path.to_owned(),
                status: "added",
            }),
    );
    changes.retain(|c| wanted(&c.path, filters));
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes.dedup_by(|a, b| a.path == b.path);
    Ok(changes)
}

/// Split `git diff` output into each file's hunks (from the first `@@`), as
/// GitHub sends a PR file's patch. Binary files get none.
fn split_patches(diff: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut path: Option<String> = None;
    let mut old: Option<String> = None;
    let mut body = String::new();
    let mut in_header = false;
    let mut flush = |path: &mut Option<String>, body: &mut String| {
        if let Some(p) = path.take()
            && !body.is_empty()
        {
            out.insert(p, std::mem::take(body));
        }
        body.clear();
    };
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            flush(&mut path, &mut body);
            in_header = true;
            old = None;
            continue;
        }
        if in_header {
            if let Some(raw) = line.strip_prefix("--- ") {
                old = header_path(raw.trim_end_matches('\n'), "a/");
                continue;
            }
            if let Some(raw) = line.strip_prefix("+++ ") {
                path = header_path(raw.trim_end_matches('\n'), "b/").or_else(|| old.clone());
                continue;
            }
            if !line.starts_with("@@ ") {
                continue;
            }
            in_header = false;
        }
        if path.is_some() {
            body.push_str(line);
        }
    }
    flush(&mut path, &mut body);
    out
}

fn header_path(raw: &str, prefix: &str) -> Option<String> {
    let raw = raw.trim_end_matches('\t');
    if raw == "/dev/null" {
        return None;
    }
    let raw = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        // Quoted names are rare with core.quotePath=false (control characters only).
        raw[1..raw.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        raw.to_owned()
    };
    Some(raw.strip_prefix(prefix).unwrap_or(&raw).to_owned())
}

/// The patch of a new, untracked file.
fn added_patch(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let mut patch = format!("@@ -0,0 +1,{} @@\n", lines.len());
    for line in lines {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    Some(patch)
}

fn blob_text(local: &Local, sha: &str) -> Option<String> {
    let out = exec_git(&git_args(&local.repo, &["cat-file", "blob", sha])).ok()?;
    if out.stdout.len() > MAX_CONTENT_BYTES || out.stdout.contains(&0) {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

struct Prepared {
    files: Vec<Value>,
    /// path → blob id of the content the review saw.
    snapshot: HashMap<String, String>,
}

fn prepare(local: &Local, base: &str, changes: &[Change]) -> Result<Prepared, AutterError> {
    let mut args: Vec<String> = git_args(
        &local.repo,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "-U3",
            "--no-color",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            base,
            "--",
        ],
    );
    // Untracked files match nothing here; they get a patch from their content below.
    args.extend(changes.iter().map(|c| format!(":(literal){}", c.path)));
    let patches = if changes.is_empty() {
        HashMap::new()
    } else {
        split_patches(&String::from_utf8_lossy(&exec_git(&args)?.stdout))
    };

    let present: Vec<String> = changes
        .iter()
        .filter(|c| c.status != "removed")
        .map(|c| c.path.clone())
        .collect();
    // Saved before the contents are read, so the review and the later line
    // mapping see exactly the same bytes.
    let snapshot = snapshot_blobs(local, &present)?;

    let mut files = Vec::new();
    let mut total = 0usize;
    for change in changes {
        let content = snapshot
            .get(&change.path)
            .and_then(|sha| blob_text(local, sha));
        let patch = match patches.get(&change.path) {
            Some(patch) => Some(patch.clone()),
            None if change.status == "added" => content.as_deref().and_then(added_patch),
            None => None,
        };
        if patch.is_none() && change.status != "removed" {
            // Binary, empty, or mode-only: nothing to review.
            continue;
        }
        total += patch.as_ref().map_or(0, String::len) + content.as_ref().map_or(0, String::len);
        files.push(json!({
            "path": change.path,
            "status": change.status,
            "patch": patch,
            "content": content,
        }));
    }
    if files.len() > MAX_FILES {
        return Err(AutterError::Generic(format!(
            "{} changed files is more than a local review takes ({MAX_FILES}). Pass the paths to review, e.g. `autter review src/payments`.",
            files.len()
        )));
    }
    if total > MAX_BYTES {
        return Err(AutterError::Generic(format!(
            "These changes are too large for one local review ({} KB of {} KB). Pass the paths to review.",
            total / 1024,
            MAX_BYTES / 1024
        )));
    }
    Ok(Prepared { files, snapshot })
}

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

/// The `result` (or `error`) line of the server's JSON-lines stream.
fn stream_result(code: u16, body: &str) -> Result<Value, AutterError> {
    if code == 401 {
        return Err(AutterError::Generic(NOT_SIGNED_IN.into()));
    }
    if code != 200 {
        let value: Option<Value> = serde_json::from_str(body).ok();
        let message = value
            .as_ref()
            .and_then(|v| v["message"].as_str().or(v["error"].as_str()))
            .map(str::to_owned);
        return Err(AutterError::Generic(match (code, message) {
            (404, None) => "The review API isn't available (HTTP 404). Update the API deployment if this command is unavailable.".into(),
            (_, Some(message)) => message,
            (_, None) => format!("HTTP {code}: the review request failed"),
        }));
    }
    let mut last_error = None;
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("result") => return Ok(event),
            Some("error") => last_error = event["message"].as_str().map(str::to_owned),
            _ => {}
        }
    }
    Err(AutterError::Generic(last_error.unwrap_or_else(|| {
        "The review ended without a result. Try again.".into()
    })))
}

/// The saved local review with `finding_id` marked dismissed (or not).
/// `Err` when the review has no such finding.
fn with_dismissal(
    mut saved: Value,
    finding_id: &str,
    dismissed: bool,
    at: &str,
) -> Result<Value, String> {
    let known = saved["result"]["findings"]
        .as_array()
        .is_some_and(|list| list.iter().any(|f| f["id"].as_str() == Some(finding_id)));
    if !known {
        return Err(format!(
            "No finding {finding_id} in the last local review on this branch. Run `autter findings --json` to see the ids."
        ));
    }
    if !saved["dismissed"].is_object() {
        saved["dismissed"] = json!({});
    }
    let map = saved["dismissed"].as_object_mut().expect("set above");
    if dismissed {
        map.insert(finding_id.to_owned(), json!({ "at": at }));
    } else {
        map.remove(finding_id);
    }
    Ok(saved)
}

/// `autter review --dismiss <finding-id> [--undo]`: tell Autter the developer
/// dismissed (or restored) one finding of their last local review, so the PR
/// page shows it as dismissed, and leave it out of `autter findings` here.
fn dismiss(options: &Options, finding_id: &str) -> Result<(), AutterError> {
    let local = local_context_for(options.repository.clone(), None)?;
    let path = local_review_path(&local);
    let saved: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .ok_or_else(|| {
            AutterError::Generic(
                "No local review on this machine yet. Run `autter review` first.".into(),
            )
        })?;
    let review_id = saved["result"]["review"]["reviewId"]
        .as_str()
        .ok_or_else(|| {
            AutterError::Generic(
                "The saved local review has no id. Run `autter review` again.".into(),
            )
        })?
        .to_owned();
    let dismissed = !options.undo;
    let updated = with_dismissal(
        saved,
        finding_id,
        dismissed,
        &chrono::Utc::now().to_rfc3339(),
    )
    .map_err(AutterError::Generic)?;

    let (context, slug) = org_api_context(options.organization.as_deref(), 30)?;
    let response = context.post_json(
        &format!(
            "/api/cli/orgs/{}/local-reviews/{}/dismissals",
            component(&slug),
            component(&review_id)
        ),
        &json!({ "findingId": finding_id, "dismissed": dismissed }),
    )?;
    let synced = response.status_code == 200;
    if !synced && response.status_code == 401 {
        return Err(AutterError::Generic(NOT_SIGNED_IN.into()));
    }
    // Kept locally either way: the editor stops showing it even if Autter is
    // unreachable (a review run before reviews were kept is unknown there: 404).
    write_if_changed(&path, &serde_json::to_string_pretty(&updated)?)?;
    let output = json!({
        "findingId": finding_id,
        "dismissed": dismissed,
        "synced": synced,
    });
    if arg_parser::json() {
        println!("{output}");
    } else if synced {
        println!(
            "{} {finding_id}. The pull request page shows it{}.",
            if dismissed { "Dismissed" } else { "Restored" },
            if dismissed { " as dismissed" } else { " again" }
        );
    } else {
        println!(
            "{} {finding_id} on this machine; Autter did not record it (HTTP {}).",
            if dismissed { "Dismissed" } else { "Restored" },
            response.status_code
        );
    }
    Ok(())
}

fn run(options: Options) -> Result<(), AutterError> {
    if let Some(finding_id) = options.dismiss.clone() {
        return dismiss(&options, &finding_id);
    }
    let local = local_context_for(options.repository.clone(), None)?;
    let (base, base_label) = resolve_base(&local, &options)?;
    let filters = repo_relative_filters(&local, &options.paths);
    let changes = changed_files(&local, &base, &filters)?;
    if changes.is_empty() {
        let message = format!(
            "No changes to review compared with {}.",
            if base_label == "HEAD" {
                "HEAD".to_owned()
            } else {
                base_label.clone()
            }
        );
        if arg_parser::json() {
            println!("{}", json!({ "localReview": null, "message": message }));
        } else {
            println!("{message}");
        }
        return Ok(());
    }
    let prepared = prepare(&local, &base, &changes)?;
    if prepared.files.is_empty() {
        println!("Nothing reviewable in these changes (binary or empty files only).");
        return Ok(());
    }

    let (context, slug) = org_api_context(options.organization.as_deref(), TIMEOUT_SECS)?;
    if !arg_parser::json() && !arg_parser::quiet() {
        eprintln!(
            "Checking {} file{} compared with {} ({})…",
            prepared.files.len(),
            if prepared.files.len() == 1 { "" } else { "s" },
            base_label,
            short_sha(&base)
        );
    }
    let title = git_text(&local, &["log", "-1", "--format=%s"])
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && base_label != "HEAD");
    let response = context.post_json(
        &format!("/api/cli/orgs/{}/review", component(&slug)),
        &json!({
            "repository": local.repository,
            "branch": local.branch,
            "title": title,
            "files": prepared.files,
        }),
    )?;
    let body = response
        .as_str()
        .map_err(|error| AutterError::Generic(error.to_string()))?;
    let result = stream_result(response.status_code, body)?;

    // Saved for `autter findings` and the editor; then placed like any later read.
    let files: BTreeMap<&String, &String> = prepared.snapshot.iter().collect();
    let saved = json!({
        "version": 1,
        "branch": local.branch,
        "base": base,
        "baseRef": base_label,
        "reviewedAt": result["review"]["reviewedAt"],
        "files": files,
        "result": result,
    });
    let path = local_review_path(&local);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_if_changed(&path, &serde_json::to_string_pretty(&saved)?)?;
    let _ = findings::exclude_findings_dir(&local.repo);
    let placed = place_local_findings(&local, &saved);

    let mut markdown_path = None;
    if options.write {
        // The file has both reviews: the PR's findings (when there is one) and this one.
        let mut combined =
            findings::collect_for_branch(&context, &slug, &local).unwrap_or_else(|_| {
                let mut stub = json!({
                    "pullRequest": null,
                    "review": null,
                    "findings": [],
                    "local": { "branch": local.branch },
                });
                findings::attach_local_review(&mut stub, &local);
                stub
            });
        if combined["localReview"].is_null() {
            findings::attach_local_review(&mut combined, &local);
        }
        markdown_path = Some(write_markdown(&local, &render_markdown(&combined))?);
    }

    let output = json!({
        "localReview": {
            "reviewId": result["review"]["reviewId"],
            "reviewedAt": result["review"]["reviewedAt"],
            "base": base,
            "baseRef": base_label,
            "files": prepared.files.len(),
            "bundles": result["review"]["bundles"],
            "ignoredPaths": result["review"]["ignoredPaths"],
            "findings": placed,
        },
        "local": {
            "branch": local.branch,
            "repository": local.repository,
            "workdir": local.workdir.to_string_lossy(),
            "markdownPath": markdown_path.as_ref().map(|p| p.to_string_lossy()),
        },
    });
    if arg_parser::json() {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    let mut out = format!(
        "Local review of {} file{} compared with {} ({})\n\n",
        prepared.files.len(),
        if prepared.files.len() == 1 { "" } else { "s" },
        one_line(&base_label),
        short_sha(&base)
    );
    render_text_findings(&mut out, &placed);
    let failed: Vec<String> = result["review"]["bundles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["error"].is_string())
        .filter_map(|b| b["bundle"].as_str().map(str::to_owned))
        .collect();
    if !failed.is_empty() {
        out.push_str(&format!(
            "\nSome checks didn't finish ({}); run autter review again to retry them.\n",
            failed.join(", ")
        ));
    }
    print!("{out}");
    if let Some(path) = markdown_path {
        println!("\nWrote {}", path.to_string_lossy());
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
        arg_parser::print_command_help("review");
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
                "src/a",
                "--base",
                "origin/dev",
                "--write",
                "--",
                "--odd"
            ]))
            .unwrap(),
            Options {
                paths: vec!["src/a".into(), "--odd".into()],
                base: Some("origin/dev".into()),
                write: true,
                ..Options::default()
            }
        );
        assert!(parse_options(&s(&["--uncommitted", "--base", "x"])).is_err());
        assert!(parse_options(&s(&["--nope"])).is_err());
        assert!(parse_options(&s(&["--base"])).is_err());
    }

    #[test]
    fn parses_name_status() {
        let out = parse_name_status("M\0src/a.ts\0A\0new file.ts\0D\0gone.ts\0T\0link\0");
        assert_eq!(
            out.iter()
                .map(|c| (c.path.as_str(), c.status))
                .collect::<Vec<_>>(),
            vec![
                ("src/a.ts", "modified"),
                ("new file.ts", "added"),
                ("gone.ts", "removed"),
                ("link", "modified"),
            ]
        );
    }

    #[test]
    fn filters_paths_and_skips_its_own_directory() {
        assert!(!wanted(".autter/findings.md", &[]));
        assert!(wanted("src/a.ts", &[]));
        assert!(wanted("src/a.ts", &["src".into()]));
        assert!(wanted("src/a.ts", &["src/".into()]));
        assert!(!wanted("srcx/a.ts", &["src".into()]));
        assert!(wanted("src/a.ts", &[".".into()]));
    }

    #[test]
    fn splits_a_diff_into_patches() {
        let diff = "\
diff --git a/src/a.ts b/src/a.ts
index 1..2 100644
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,2 +1,2 @@
 keep
--- removed sql comment
+added
diff --git a/gone.ts b/gone.ts
deleted file mode 100644
--- a/gone.ts
+++ /dev/null
@@ -1 +0,0 @@
-x
diff --git a/img.png b/img.png
Binary files a/img.png and b/img.png differ
";
        let patches = split_patches(diff);
        assert_eq!(
            patches["src/a.ts"],
            "@@ -1,2 +1,2 @@\n keep\n--- removed sql comment\n+added\n"
        );
        assert_eq!(patches["gone.ts"], "@@ -1 +0,0 @@\n-x\n");
        assert!(!patches.contains_key("img.png"));
    }

    #[test]
    fn builds_a_patch_for_a_new_file() {
        assert_eq!(added_patch("a\nb\n").unwrap(), "@@ -0,0 +1,2 @@\n+a\n+b\n");
        assert_eq!(added_patch(""), None);
    }

    #[test]
    fn reads_the_result_line_of_the_stream() {
        let body = "{\"type\":\"progress\",\"stage\":\"started\"}\n{\"type\":\"progress\",\"keepalive\":true}\n{\"type\":\"result\",\"findings\":[]}\n";
        assert_eq!(stream_result(200, body).unwrap()["type"], "result");
        let err = stream_result(
            200,
            "{\"type\":\"error\",\"message\":\"The review failed.\"}\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("The review failed."), "{err}");
        assert!(stream_result(200, "{\"type\":\"progress\"}\n").is_err());
        let err = stream_result(
            429,
            "{\"error\":\"rate_limited\",\"message\":\"Rate limit exceeded\"}",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Rate limit exceeded"), "{err}");
        assert!(
            stream_result(401, "")
                .unwrap_err()
                .to_string()
                .contains("autter login")
        );
    }

    #[test]
    fn parses_dismiss_options() {
        let o = parse_options(&s(&["--dismiss", "local-abc-1", "--undo"])).unwrap();
        assert_eq!(o.dismiss.as_deref(), Some("local-abc-1"));
        assert!(o.undo);
        assert!(parse_options(&s(&["--undo"])).is_err());
        assert!(parse_options(&s(&["--dismiss"])).is_err());
    }

    #[test]
    fn records_and_clears_a_dismissal_in_the_saved_review() {
        let saved =
            json!({ "result": { "findings": [{ "id": "local-abc-1" }, { "id": "local-abc-2" }] } });
        let once = with_dismissal(saved, "local-abc-1", true, "2026-11-17T10:00:00Z").unwrap();
        assert_eq!(
            once["dismissed"]["local-abc-1"]["at"],
            "2026-11-17T10:00:00Z"
        );
        let cleared = with_dismissal(once, "local-abc-1", false, "x").unwrap();
        assert!(cleared["dismissed"]["local-abc-1"].is_null());
        assert!(with_dismissal(cleared, "local-zzz-9", true, "x").is_err());
    }
}
