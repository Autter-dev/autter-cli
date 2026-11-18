//! `autter sandbox`: build, test and rule-check a pushed branch in Autter's
//! sandbox, without opening a pull request.
//!
//! `autter sandbox run` asks `POST /api/cli/orgs/<org>/sandbox-runs` to run
//! the branch's commit: Autter clones it from GitHub (nothing is uploaded from
//! this machine), runs the repository's own build and test commands, and runs
//! the same rule-based checks as `autter review` over the branch's changes
//! against the default branch. It makes no AI calls and posts nothing to
//! GitHub: the result is on the run's dashboard page and here, by polling
//! `GET /api/cli/orgs/<org>/sandbox-runs/<id>`. `status <id>` and `list` read
//! runs back.
//!
//! The branch must be pushed: the CLI refuses when the branch has no upstream
//! or has commits its upstream lacks, and the server checks GitHub has the
//! same commit.
use crate::api::client::ApiContext;
use crate::commands::arg_parser::{self, ScanMode};
use crate::commands::ask::{component, org_api_context};
use crate::commands::findings::{Local, git_args, local_context_for, short_sha};
use crate::error::AutterError;
use crate::git::repository::exec_git_allow_nonzero;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const TIMEOUT_SECS: u64 = 60;
/// How often `run` asks how the run is going, and for how long at most.
const POLL_EVERY: Duration = Duration::from_secs(5);
const WAIT_AT_MOST: Duration = Duration::from_secs(60 * 60);

/// The checks a sandbox run takes. All deterministic: no AI calls.
pub(crate) const CHECKS: [&str; 3] = ["build", "test", "rules"];

const NOT_SIGNED_IN: &str = "Autter did not accept your sign-in for this organization (HTTP 401). Sign in with `autter login` first, or choose an organization with --org <slug>.";

#[derive(Debug, PartialEq)]
enum Action {
    Run,
    Status(String),
    List,
}

#[derive(Debug, PartialEq)]
struct Options {
    action: Action,
    branch: Option<String>,
    checks: Vec<String>,
    repository: Option<String>,
    organization: Option<String>,
    no_wait: bool,
    /// `list`: every repository's runs, not only this one's.
    all: bool,
    limit: Option<u32>,
}

/// `--checks build,test` → the checks, in their usual order; walk and others refused.
fn parse_checks(value: &str) -> Result<Vec<String>, String> {
    let wanted: Vec<String> = value
        .split(',')
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .collect();
    for check in &wanted {
        if check == "walk" {
            return Err(
                "the product walk is not part of a sandbox run: sandbox runs make no AI calls. Use --checks build,test,rules".into(),
            );
        }
        if !CHECKS.contains(&check.as_str()) {
            return Err(format!(
                "unknown check \"{check}\"; choose from {}",
                CHECKS.join(", ")
            ));
        }
    }
    let checks: Vec<String> = CHECKS
        .iter()
        .filter(|c| wanted.iter().any(|w| w == *c))
        .map(|c| (*c).to_owned())
        .collect();
    if checks.is_empty() {
        return Err(format!(
            "--checks needs at least one of {}",
            CHECKS.join(", ")
        ));
    }
    Ok(checks)
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let (action, rest) = match args.first().map(String::as_str) {
        Some("run") => (Action::Run, &args[1..]),
        Some("list") | Some("ls") => (Action::List, &args[1..]),
        Some("status") => {
            let id = args
                .get(1)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty() && !v.starts_with('-'))
                .ok_or("status needs a run id: autter sandbox status <id>")?;
            (Action::Status(id.to_owned()), &args[2..])
        }
        Some(other) => {
            return Err(format!(
                "unknown sandbox command: {other} (use run, status <id> or list)"
            ));
        }
        None => return Err("choose one: autter sandbox run | status <id> | list".into()),
    };
    let mut options = Options {
        action,
        branch: None,
        checks: CHECKS.iter().map(|c| (*c).to_owned()).collect(),
        repository: None,
        organization: None,
        no_wait: false,
        all: false,
        limit: None,
    };
    let mut index = 0;
    while index < rest.len() {
        let arg = rest[index].as_str();
        match arg {
            "--no-wait" => options.no_wait = true,
            "--all" => options.all = true,
            "--branch" | "--checks" | "--repo" | "--org" | "--limit" => {
                let value = rest
                    .get(index + 1)
                    .map(|v| v.trim())
                    .filter(|v| !v.starts_with("--") && !v.is_empty())
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                match arg {
                    "--branch" => options.branch = Some(value.to_owned()),
                    "--checks" => options.checks = parse_checks(value)?,
                    "--repo" => options.repository = Some(value.to_owned()),
                    "--org" => options.organization = Some(value.to_owned()),
                    _ => {
                        let n: u32 = value
                            .parse()
                            .ok()
                            .filter(|n| (1..=100).contains(n))
                            .ok_or("--limit must be a number from 1 to 100")?;
                        options.limit = Some(n);
                    }
                }
                index += 2;
                continue;
            }
            _ => return Err(format!("unknown option for sandbox: {arg}")),
        }
        index += 1;
    }
    match options.action {
        Action::Run => {
            if options.all || options.limit.is_some() {
                return Err("--all and --limit go with autter sandbox list".into());
            }
        }
        Action::Status(_) | Action::List => {
            if options.branch.is_some() || options.no_wait {
                return Err("--branch and --no-wait go with autter sandbox run".into());
            }
        }
    }
    Ok(options)
}

// ---------------------------------------------------------------------------
// Is the branch pushed?
// ---------------------------------------------------------------------------

fn git_line(local: &Local, rest: &[&str]) -> Option<String> {
    let out = exec_git_allow_nonzero(&git_args(&local.repo, rest)).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// What the developer has against what their upstream has.
#[derive(Debug, PartialEq)]
enum PushState {
    /// The branch exists here: its commit, which must also be the upstream's.
    Pushed(String),
    /// No upstream to compare with.
    NoUpstream,
    /// Commits here the upstream lacks.
    Ahead {
        local: String,
        upstream: String,
        upstream_name: String,
    },
    /// No such local branch: the server checks GitHub has it.
    NotLocal,
}

fn push_state(
    local_head: Option<String>,
    upstream: Option<(String, String)>,
    ahead: Option<u32>,
) -> PushState {
    let Some(head) = local_head else {
        return PushState::NotLocal;
    };
    let Some((upstream_name, upstream_sha)) = upstream else {
        return PushState::NoUpstream;
    };
    if head == upstream_sha || ahead == Some(0) {
        // Behind (or equal) is fine: the run is of what is on GitHub.
        return PushState::Pushed(head);
    }
    PushState::Ahead {
        local: head,
        upstream: upstream_sha,
        upstream_name,
    }
}

fn branch_push_state(local: &Local, branch: &str) -> PushState {
    let head = git_line(
        local,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}^{{commit}}"),
        ],
    );
    let upstream_name = git_line(
        local,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &format!("{branch}@{{upstream}}"),
        ],
    );
    let upstream = upstream_name.and_then(|name| {
        git_line(
            local,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{name}^{{commit}}"),
            ],
        )
        .map(|sha| (name, sha))
    });
    let ahead = match (&head, &upstream) {
        (Some(_), Some((name, _))) => git_line(
            local,
            &[
                "rev-list",
                "--count",
                &format!("{name}..refs/heads/{branch}"),
            ],
        )
        .and_then(|n| n.parse().ok()),
        _ => None,
    };
    push_state(head, upstream, ahead)
}

fn not_pushed_message(state: &PushState, branch: &str) -> Option<String> {
    match state {
        PushState::NoUpstream => Some(format!(
            "{branch} has no upstream, so Autter can't see it. Push it first: git push -u origin {branch}"
        )),
        PushState::Ahead {
            local,
            upstream,
            upstream_name,
        } => Some(format!(
            "{branch} is at {} but {upstream_name} is at {}: a sandbox run builds what is on GitHub. Push first (git push), then run it again.",
            short_sha(local),
            short_sha(upstream)
        )),
        PushState::Pushed(_) | PushState::NotLocal => None,
    }
}

// ---------------------------------------------------------------------------
// Talking to Autter
// ---------------------------------------------------------------------------

fn response_value(status: u16, body: &str) -> Result<Value, AutterError> {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(value);
    }
    if status == 401 {
        return Err(AutterError::Generic(NOT_SIGNED_IN.into()));
    }
    let message = value["message"]
        .as_str()
        .or_else(|| value["error"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Autter answered HTTP {status}."));
    Err(AutterError::Generic(message))
}

fn start(context: &ApiContext, slug: &str, body: &Value) -> Result<Value, AutterError> {
    let response = context.post_json(
        &format!("/api/cli/orgs/{}/sandbox-runs", component(slug)),
        body,
    )?;
    let text = response
        .as_str()
        .map_err(|error| AutterError::Generic(error.to_string()))?;
    response_value(response.status_code, text)
}

fn fetch_run(context: &ApiContext, slug: &str, id: &str) -> Result<Value, AutterError> {
    let response = context.get(&format!(
        "/api/cli/orgs/{}/sandbox-runs/{}",
        component(slug),
        component(id)
    ))?;
    let text = response
        .as_str()
        .map_err(|error| AutterError::Generic(error.to_string()))?;
    response_value(response.status_code, text)
}

fn fetch_list(
    context: &ApiContext,
    slug: &str,
    repository: Option<&str>,
    limit: Option<u32>,
) -> Result<Value, AutterError> {
    let mut query = Vec::new();
    if let Some(repository) = repository {
        query.push(format!("repository={}", component(repository)));
    }
    if let Some(limit) = limit {
        query.push(format!("limit={limit}"));
    }
    let path = format!(
        "/api/cli/orgs/{}/sandbox-runs{}{}",
        component(slug),
        if query.is_empty() { "" } else { "?" },
        query.join("&")
    );
    let response = context.get(&path)?;
    let text = response
        .as_str()
        .map_err(|error| AutterError::Generic(error.to_string()))?;
    response_value(response.status_code, text)
}

fn is_active(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("pending") | Some("running"))
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn headline(run: &Value) -> String {
    match run["status"].as_str() {
        Some("pending") => "Queued: waiting for a sandbox.".into(),
        Some("running") => "Running the checks now.".into(),
        Some("cancelled") => "Cancelled.".into(),
        Some("failed") => run["error"]
            .as_str()
            .unwrap_or("The run could not finish.")
            .to_owned(),
        _ => match run["result"]["verdict"].as_str() {
            Some("passed") => "Everything that ran passed.".into(),
            Some("failed") => "Something failed.".into(),
            Some("incomplete") => "Some checks could not run.".into(),
            _ => "Finished.".into(),
        },
    }
}

fn stage_word(status: &str) -> &str {
    match status {
        "passed" => "passed",
        "failed" => "FAILED",
        "error" => "could not run",
        _ => "skipped",
    }
}

fn render_commands(out: &mut String, stage: &Value) {
    for command in stage["commands"].as_array().into_iter().flatten() {
        let dir = command["dir"]
            .as_str()
            .filter(|d| !d.is_empty())
            .unwrap_or(".");
        out.push_str(&format!(
            "    {:<7} {} (in {dir})",
            stage_word(command["status"].as_str().unwrap_or("skipped")),
            command["command"].as_str().unwrap_or("")
        ));
        if let Some(reason) = command["reason"].as_str() {
            out.push_str(&format!(": {reason}"));
        }
        out.push('\n');
    }
}

/// A finished (or running) run as text.
fn render_run(run: &Value) -> String {
    let mut out = format!(
        "Sandbox run {} of {} ({})\n{}\n",
        run["id"].as_str().unwrap_or("?"),
        run["branch"].as_str().unwrap_or("?"),
        short_sha(run["headSha"].as_str().unwrap_or("")),
        headline(run)
    );
    let result = &run["result"];
    for (key, title) in [("build", "Build"), ("test", "Tests")] {
        let stage = &result[key];
        if stage.is_null() {
            continue;
        }
        out.push_str(&format!(
            "\n  {title}: {}. {}\n",
            stage_word(stage["status"].as_str().unwrap_or("skipped")),
            stage["summary"].as_str().unwrap_or("")
        ));
        render_commands(&mut out, stage);
    }
    let rules = &result["rules"];
    if !rules.is_null() {
        out.push_str(&format!(
            "\n  Rule-based checks: {}. {}\n",
            stage_word(rules["status"].as_str().unwrap_or("skipped")),
            rules["summary"].as_str().unwrap_or("")
        ));
        for finding in rules["findings"].as_array().into_iter().flatten() {
            let line = finding["line"].as_u64().filter(|l| *l > 0);
            out.push_str(&format!(
                "    {:<8} {}{}  {}\n",
                finding["severity"].as_str().unwrap_or(""),
                finding["path"].as_str().unwrap_or(""),
                line.map(|l| format!(":{l}")).unwrap_or_default(),
                finding["title"].as_str().unwrap_or("")
            ));
        }
    }
    out.push_str("\nNo AI calls. Nothing was posted to GitHub.\n");
    if let Some(url) = run["url"].as_str() {
        out.push_str(&format!("Details: {url}\n"));
    }
    out
}

fn render_list(list: &Value) -> String {
    let runs: Vec<&Value> = list["runs"].as_array().into_iter().flatten().collect();
    if runs.is_empty() {
        return "No sandbox runs yet. Start one with: autter sandbox run\n".into();
    }
    let mut out = String::new();
    for run in runs {
        out.push_str(&format!(
            "{}  {:<9} {:<24} {}  {}\n",
            run["id"].as_str().unwrap_or("?"),
            run["status"].as_str().unwrap_or("?"),
            run["branch"].as_str().unwrap_or("?"),
            short_sha(run["headSha"].as_str().unwrap_or("")),
            headline(run)
        ));
    }
    out
}

/// 0 when everything that ran passed (or the run is still going with
/// --no-wait); 1 when a check failed, could not run, or the run did not finish.
fn exit_code_for(run: &Value) -> i32 {
    if is_active(run) {
        return 0;
    }
    match (run["status"].as_str(), run["result"]["verdict"].as_str()) {
        (Some("done"), Some("passed")) => 0,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn wait_for(context: &ApiContext, slug: &str, mut run: Value) -> Result<Value, AutterError> {
    let id = run["id"].as_str().unwrap_or_default().to_owned();
    let started = Instant::now();
    let mut last = run["status"].as_str().unwrap_or_default().to_owned();
    while is_active(&run) {
        if started.elapsed() > WAIT_AT_MOST {
            return Err(AutterError::Generic(format!(
                "Still running after an hour. Check later with: autter sandbox status {id}"
            )));
        }
        std::thread::sleep(POLL_EVERY);
        run = fetch_run(context, slug, &id)?;
        let status = run["status"].as_str().unwrap_or_default().to_owned();
        if status != last && !arg_parser::json() && !arg_parser::quiet() {
            eprintln!("{}", headline(&run));
        }
        last = status;
    }
    Ok(run)
}

fn run_sandbox(options: &Options) -> Result<i32, AutterError> {
    let local = local_context_for(options.repository.clone(), options.branch.clone())?;
    let branch = local.branch.clone().ok_or_else(|| {
        AutterError::Generic(
            "HEAD is detached. Check out a branch, or pass --branch <name>.".into(),
        )
    })?;
    let state = branch_push_state(&local, &branch);
    if let Some(message) = not_pushed_message(&state, &branch) {
        return Err(AutterError::Generic(message));
    }
    let head_sha = match state {
        PushState::Pushed(sha) => Some(sha),
        _ => None,
    };
    let current = local
        .repo
        .head()
        .ok()
        .and_then(|head| head.shorthand().ok());
    if current.as_deref() == Some(branch.as_str())
        && git_line(&local, &["status", "--porcelain", "--untracked-files=no"]).is_some()
        && !arg_parser::json()
        && !arg_parser::quiet()
    {
        eprintln!(
            "Note: uncommitted changes are not part of a sandbox run; it runs what is on GitHub."
        );
    }

    let (context, slug) = org_api_context(options.organization.as_deref(), TIMEOUT_SECS)?;
    let queued = start(
        &context,
        &slug,
        &json!({
            "repository": local.repository,
            "branch": branch,
            "headSha": head_sha,
            "checks": options.checks,
        }),
    )?;
    if !arg_parser::json() && !arg_parser::quiet() {
        eprintln!(
            "Sandbox run {} queued for {branch} ({}): {}. No AI calls.",
            queued["id"].as_str().unwrap_or("?"),
            short_sha(queued["headSha"].as_str().unwrap_or("")),
            options.checks.join(", ")
        );
        if let Some(url) = queued["url"].as_str() {
            eprintln!("Follow it at {url}");
        }
    }
    let run = if options.no_wait {
        queued
    } else {
        wait_for(&context, &slug, queued)?
    };
    if arg_parser::json() {
        println!("{}", serde_json::to_string_pretty(&run)?);
    } else if options.no_wait {
        println!(
            "Check on it with: autter sandbox status {}",
            run["id"].as_str().unwrap_or("?")
        );
    } else {
        print!("{}", render_run(&run));
    }
    Ok(exit_code_for(&run))
}

fn run(options: Options) -> Result<i32, AutterError> {
    match &options.action {
        Action::Run => run_sandbox(&options),
        Action::Status(id) => {
            let (context, slug) = org_api_context(options.organization.as_deref(), TIMEOUT_SECS)?;
            let run = fetch_run(&context, &slug, id)?;
            if arg_parser::json() {
                println!("{}", serde_json::to_string_pretty(&run)?);
            } else {
                print!("{}", render_run(&run));
            }
            Ok(exit_code_for(&run))
        }
        Action::List => {
            let repository = if options.all {
                None
            } else {
                match options.repository.clone() {
                    Some(repository) => Some(repository),
                    None => local_context_for(None, None)
                        .ok()
                        .map(|local| local.repository),
                }
            };
            let (context, slug) = org_api_context(options.organization.as_deref(), TIMEOUT_SECS)?;
            let list = fetch_list(&context, &slug, repository.as_deref(), options.limit)?;
            if arg_parser::json() {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                print!("{}", render_list(&list));
            }
            Ok(0)
        }
    }
}

pub fn handle(args: &[String]) {
    let parsed = arg_parser::pre_parse(args, ScanMode::Full, true).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        std::process::exit(2);
    });
    arg_parser::merge_global_flags(&parsed.flags);
    if parsed.flags.help {
        arg_parser::print_command_help("sandbox");
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
    match run(options) {
        Ok(code) => {
            if code != 0 {
                std::process::exit(code);
            }
        }
        Err(error) => {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }

    fn all_checks() -> Vec<String> {
        CHECKS.iter().map(|c| (*c).to_owned()).collect()
    }

    #[test]
    fn parses_run_with_its_options() {
        let options = parse_options(&s(&[
            "run",
            "--branch",
            "feat/x",
            "--checks",
            "rules,build",
            "--no-wait",
            "--org",
            "acme",
        ]))
        .unwrap();
        assert_eq!(options.action, Action::Run);
        assert_eq!(options.branch.as_deref(), Some("feat/x"));
        assert_eq!(options.checks, vec!["build".to_owned(), "rules".to_owned()]);
        assert!(options.no_wait);
        assert_eq!(options.organization.as_deref(), Some("acme"));
        assert_eq!(parse_options(&s(&["run"])).unwrap().checks, all_checks());
    }

    #[test]
    fn refuses_the_walk_and_unknown_checks() {
        let walk = parse_options(&s(&["run", "--checks", "build,walk"])).unwrap_err();
        assert!(walk.contains("no AI calls"), "{walk}");
        assert!(parse_options(&s(&["run", "--checks", "lint"])).is_err());
        assert!(parse_options(&s(&["run", "--checks", ","])).is_err());
    }

    #[test]
    fn parses_status_and_list() {
        assert_eq!(
            parse_options(&s(&["status", "sbx_1"])).unwrap().action,
            Action::Status("sbx_1".into())
        );
        assert!(parse_options(&s(&["status"])).is_err());
        let list = parse_options(&s(&["list", "--all", "--limit", "5"])).unwrap();
        assert_eq!(list.action, Action::List);
        assert!(list.all);
        assert_eq!(list.limit, Some(5));
        assert!(parse_options(&s(&["list", "--no-wait"])).is_err());
        assert!(parse_options(&s(&["run", "--all"])).is_err());
        assert!(parse_options(&s(&["walk"])).is_err());
        assert!(parse_options(&s(&[])).is_err());
    }

    #[test]
    fn requires_the_branch_to_be_pushed() {
        let up = |sha: &str| Some(("origin/feat".to_owned(), sha.to_owned()));
        assert_eq!(
            push_state(Some("a".repeat(40)), up(&"a".repeat(40)), Some(0)),
            PushState::Pushed("a".repeat(40))
        );
        // Behind the upstream is fine: the run is of what is on GitHub.
        assert_eq!(
            push_state(Some("a".repeat(40)), up(&"b".repeat(40)), Some(0)),
            PushState::Pushed("a".repeat(40))
        );
        let ahead = push_state(Some("a".repeat(40)), up(&"b".repeat(40)), Some(2));
        assert!(
            not_pushed_message(&ahead, "feat")
                .unwrap()
                .contains("Push first (git push)")
        );
        let none = push_state(Some("a".repeat(40)), None, None);
        assert!(
            not_pushed_message(&none, "feat")
                .unwrap()
                .contains("git push -u origin feat")
        );
        assert_eq!(push_state(None, None, None), PushState::NotLocal);
        assert_eq!(not_pushed_message(&PushState::NotLocal, "feat"), None);
    }

    fn finished() -> Value {
        json!({
            "id": "sbx_1",
            "branch": "feat/billing",
            "headSha": "abcdef1234567890abcdef1234567890abcdef12",
            "status": "done",
            "url": "https://app.autter.dev/acme/repositories/web/sandbox-runs/sbx_1",
            "result": {
                "verdict": "failed",
                "noAiCalls": true,
                "build": { "status": "passed", "summary": "1 build passed.", "commands": [
                    { "name": "web", "dir": "", "command": "npm run build", "status": "passed" }
                ]},
                "test": { "status": "failed", "summary": "1 of 1 test suite failed.", "commands": [
                    { "name": "web", "dir": "web", "command": "npm test", "status": "failed", "reason": "code" }
                ]},
                "rules": { "status": "passed", "summary": "1 finding, none critical.", "findings": [
                    { "severity": "warning", "path": "src/a.ts", "line": 4, "title": "Password written to logs" }
                ]}
            }
        })
    }

    #[test]
    fn renders_a_finished_run() {
        let text = render_run(&finished());
        assert!(
            text.contains("Sandbox run sbx_1 of feat/billing (abcdef1)"),
            "{text}"
        );
        assert!(text.contains("Something failed."));
        assert!(text.contains("Build: passed. 1 build passed."));
        assert!(text.contains("FAILED  npm test (in web): code"));
        assert!(text.contains("warning  src/a.ts:4  Password written to logs"));
        assert!(text.contains("No AI calls. Nothing was posted to GitHub."));
        assert!(
            text.contains(
                "Details: https://app.autter.dev/acme/repositories/web/sandbox-runs/sbx_1"
            )
        );
    }

    #[test]
    fn exits_non_zero_unless_everything_passed() {
        assert_eq!(exit_code_for(&finished()), 1);
        let mut passed = finished();
        passed["result"]["verdict"] = json!("passed");
        assert_eq!(exit_code_for(&passed), 0);
        assert_eq!(exit_code_for(&json!({ "status": "pending" })), 0);
        assert_eq!(
            exit_code_for(&json!({ "status": "failed", "error": "x" })),
            1
        );
    }

    #[test]
    fn lists_runs_or_says_how_to_start_one() {
        assert!(render_list(&json!({ "runs": [] })).contains("autter sandbox run"));
        let text = render_list(&json!({ "runs": [finished()] }));
        assert!(text.contains("sbx_1"));
        assert!(text.contains("feat/billing"));
    }

    fn context(server: &mockito::Server) -> ApiContext {
        ApiContext::with_auth(Some(server.url()), "test-access-token".to_string())
    }

    #[test]
    fn starts_and_reads_runs_with_the_cli_token() {
        let mut server = mockito::Server::new();
        let created = server
            .mock("POST", "/api/cli/orgs/acme/sandbox-runs")
            .match_header("authorization", "Bearer test-access-token")
            .match_body(mockito::Matcher::PartialJson(json!({
                "repository": "acme/web",
                "branch": "feat/billing",
                "checks": ["build", "test", "rules"],
            })))
            .with_status(202)
            .with_body(json!({ "id": "sbx_1", "status": "pending" }).to_string())
            .create();
        let body = json!({ "repository": "acme/web", "branch": "feat/billing", "headSha": null, "checks": CHECKS });
        let queued = start(&context(&server), "acme", &body).unwrap();
        created.assert();
        assert_eq!(queued["id"], "sbx_1");

        let read = server
            .mock("GET", "/api/cli/orgs/acme/sandbox-runs/sbx_1")
            .with_status(200)
            .with_body(finished().to_string())
            .create();
        let run = fetch_run(&context(&server), "acme", "sbx_1").unwrap();
        read.assert();
        assert_eq!(run["result"]["verdict"], "failed");
    }

    #[test]
    fn says_why_autter_refused() {
        let mut server = mockito::Server::new();
        let cases = [
            (401, r#"{"error":"unauthenticated"}"#, "autter login"),
            (
                409,
                r#"{"error":"branch_not_pushed","message":"feat is not on GitHub. Push it first: git push -u origin feat"}"#,
                "git push -u origin feat",
            ),
            (
                429,
                r#"{"error":"sandbox_busy","message":"A sandbox run of acme/web is already queued or running."}"#,
                "already queued or running",
            ),
        ];
        for (status, body, expected) in cases {
            let mock = server
                .mock("POST", "/api/cli/orgs/acme/sandbox-runs")
                .with_status(status)
                .with_body(body)
                .create();
            let error = start(&context(&server), "acme", &json!({})).unwrap_err();
            mock.assert();
            assert!(error.to_string().contains(expected), "{error}");
            mock.remove();
        }
    }
}
