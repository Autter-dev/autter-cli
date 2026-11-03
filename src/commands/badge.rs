//! `autter badge`: a README badge reading `AI-written | 72%`.
//!
//! The number is the repo-wide share from `authorship::repo_share`. The
//! snippet is one of:
//!
//! * a live badge served by Autter, when the repository is linked to an
//!   organization and the user is signed in (`POST /cli/repo-badge-link`
//!   hands back the public token behind `/api/public/repos/<token>/badge.svg`);
//! * a static shields.io badge carrying the local number, for everyone else.
//!
//! `--json` prints the share as a shields.io endpoint payload, and `--write`
//! stores that payload in the repository (default `.autter/badge.json`) so a
//! committed file can back the badge without an account.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::client::{
    ApiClient, ApiContext, access_token_for_org, resolve_org_for_repo_cached,
};
use crate::authorship::ignore::effective_ignore_patterns;
use crate::authorship::repo_share::{RepoShare, Window, repo_share};
use crate::commands::arg_parser::{self, ScanMode};
use crate::error::AutterError;
use crate::git::repository::{Repository, find_repository};
use crate::repo_url::resolve_repo_url_from_repo;

/// Badge label, left of the separator.
pub const LABEL: &str = "AI-written";
/// Where a static badge links to.
pub const SITE_URL: &str = "https://autter.dev";
/// Default `--write` target, relative to the repository root.
pub const DEFAULT_WRITE_PATH: &str = ".autter/badge.json";
/// How long the live-badge lookup may take before the static snippet is used.
const CLOUD_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default, PartialEq)]
struct Options {
    window: Option<Window>,
    write: Option<PathBuf>,
    ignore_patterns: Vec<String>,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--since" => {
                let value = args
                    .get(index + 1)
                    .ok_or("--since requires a date or duration")?;
                set_window(&mut options, Window::Since(value.clone()))?;
                index += 2;
            }
            "--all-time" => {
                set_window(&mut options, Window::AllTime)?;
                index += 1;
            }
            "--write" => {
                let next = args.get(index + 1).filter(|next| !next.starts_with('-'));
                options.write = Some(PathBuf::from(
                    next.map(String::as_str).unwrap_or(DEFAULT_WRITE_PATH),
                ));
                index += if next.is_some() { 2 } else { 1 };
            }
            "--ignore" => {
                index += 1;
                let start = index;
                while index < args.len() && !args[index].starts_with("--") {
                    options.ignore_patterns.push(args[index].clone());
                    index += 1;
                }
                if index == start {
                    return Err("--ignore requires at least one pattern".into());
                }
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--since=") {
                    set_window(&mut options, Window::Since(value.to_string()))?;
                } else if let Some(path) = arg.strip_prefix("--write=") {
                    options.write = Some(PathBuf::from(path));
                } else {
                    return Err(format!("unknown argument: {arg}"));
                }
                index += 1;
            }
        }
    }
    Ok(options)
}

fn set_window(options: &mut Options, window: Window) -> Result<(), String> {
    if options.window.is_some() {
        return Err("use only one of --since and --all-time".into());
    }
    options.window = Some(window);
    Ok(())
}

/// Shields color for a share: lightgrey with no data, then four steps from
/// light blue up to Autter's teal.
pub fn badge_color(share: &RepoShare) -> &'static str {
    if !share.has_data() {
        "lightgrey"
    } else if share.ai_percent < 25.0 {
        "74c0fc"
    } else if share.ai_percent < 50.0 {
        "007ec6"
    } else if share.ai_percent < 75.0 {
        "5b5bd6"
    } else {
        "2ee6c5"
    }
}

/// Badge message, right of the separator.
pub fn badge_message(share: &RepoShare) -> String {
    if share.has_data() {
        format!("{}%", share.ai_percent.round() as u64)
    } else {
        "no data".to_string()
    }
}

/// The share as a shields.io endpoint payload, with the figures behind it.
/// Field order is the written order: the shields keys come first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BadgePayload {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub label: String,
    pub message: String,
    pub color: String,
    pub ai_percent: f64,
    pub ai_accepted: u64,
    pub added_lines: u64,
    pub commits: usize,
    pub commits_with_authorship: usize,
    pub since: Option<String>,
    pub capped: bool,
}

/// The shields.io endpoint payload for a share.
pub fn badge_payload(share: &RepoShare) -> BadgePayload {
    BadgePayload {
        schema_version: 1,
        label: LABEL.to_string(),
        message: badge_message(share),
        color: badge_color(share).to_string(),
        ai_percent: share.ai_percent,
        ai_accepted: share.ai_accepted,
        added_lines: share.added_lines,
        commits: share.commits,
        commits_with_authorship: share.commits_with_authorship,
        since: share.since.clone(),
        capped: share.capped,
    }
}

/// A shields.io static badge URL for the share.
pub fn static_badge_url(share: &RepoShare) -> String {
    format!(
        "https://img.shields.io/badge/{}-{}-{}",
        shields_segment(LABEL),
        shields_segment(&badge_message(share)),
        badge_color(share)
    )
}

/// Escape one segment of a shields.io static badge path: `-` becomes `--`,
/// `_` becomes `__`, and the rest is percent-encoded.
fn shields_segment(text: &str) -> String {
    let escaped = text.replace('-', "--").replace('_', "__").replace(' ', "_");
    url::form_urlencoded::byte_serialize(escaped.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// A shields.io endpoint badge URL reading the payload at `raw_url`.
pub fn endpoint_badge_url(raw_url: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(raw_url.as_bytes()).collect();
    format!("https://img.shields.io/endpoint?url={encoded}")
}

/// Markdown for a badge image linking to `link`.
pub fn markdown_snippet(badge_url: &str, link: &str) -> String {
    format!("[![{LABEL}]({badge_url})]({link})")
}

/// The raw-file URL GitHub serves for `path` on `branch`, when the remote is a
/// GitHub repository.
pub fn github_raw_url(repo_url: &str, branch: &str, path: &str) -> Option<String> {
    let rest = repo_url.strip_prefix("https://github.com/")?;
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(format!(
        "https://raw.githubusercontent.com/{owner}/{name}/{branch}/{}",
        path.trim_start_matches('/')
    ))
}

/// A live badge served by Autter.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CloudBadge {
    badge_url: String,
    page_url: String,
}

/// Resolve the live badge for `repo`, or say in one line why the static
/// snippet is used instead. Never waits longer than [`CLOUD_TIMEOUT`].
fn cloud_badge(repo: &Repository) -> Result<CloudBadge, String> {
    let repo_url = resolve_repo_url_from_repo(repo)
        .ok_or_else(|| "this repository has no remote to link to Autter".to_string())?;
    if !crate::config::platform_sync_enabled_now() {
        return Err("autter is in local mode".to_string());
    }
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(request_cloud_badge(&repo_url));
    });
    match receiver.recv_timeout(CLOUD_TIMEOUT) {
        Ok(result) => result,
        Err(_) => Err(format!(
            "Autter did not answer within {}s",
            CLOUD_TIMEOUT.as_secs()
        )),
    }
}

fn request_cloud_badge(repo_url: &str) -> Result<CloudBadge, String> {
    let org = resolve_org_for_repo_cached(repo_url).ok_or_else(|| {
        "sign in with `autter login` and link this repository to an organization".to_string()
    })?;
    let token = access_token_for_org(&org)
        .ok_or_else(|| "could not get an access token; run `autter login` again".to_string())?;
    let client =
        ApiClient::new(ApiContext::with_auth(None, token).with_timeout(CLOUD_TIMEOUT.as_secs()));
    let link = client
        .create_repo_badge_link(repo_url)
        .map_err(|e| e.to_string())?;
    let base = client.context().base_url.trim_end_matches('/').to_string();
    Ok(CloudBadge {
        badge_url: link
            .badge_url
            .unwrap_or_else(|| format!("{base}/api/public/repos/{}/badge.svg", link.token)),
        page_url: link
            .page_url
            .unwrap_or_else(crate::commands::login::web_app_url),
    })
}

/// The README snippet and, when the live badge is unavailable, why.
fn snippet_for(
    repo: &Repository,
    share: &RepoShare,
    written: Option<&str>,
) -> (String, Vec<String>) {
    let mut notes = Vec::new();
    match cloud_badge(repo) {
        Ok(cloud) => (markdown_snippet(&cloud.badge_url, &cloud.page_url), notes),
        Err(reason) => {
            if let Some(path) = written {
                let branch = repo
                    .head()
                    .ok()
                    .and_then(|head| head.shorthand().ok())
                    .unwrap_or_else(|| "main".to_string());
                if let Some(raw) = resolve_repo_url_from_repo(repo)
                    .and_then(|url| github_raw_url(&url, &branch, path))
                {
                    notes.push(format!(
                        "Static badge: {LABEL} reads {path} once it is committed and pushed ({reason})."
                    ));
                    return (markdown_snippet(&endpoint_badge_url(&raw), SITE_URL), notes);
                }
                notes.push(format!(
                    "Static badge: commit {path} and point shields.io at its raw URL: https://img.shields.io/endpoint?url=<raw url> ({reason})."
                ));
            } else {
                notes.push(format!(
                    "Static badge with today's number ({reason}). Run `autter badge --write` for one that updates from a committed file."
                ));
            }
            (markdown_snippet(&static_badge_url(share), SITE_URL), notes)
        }
    }
}

fn write_payload(
    repo: &Repository,
    path: &Path,
    payload: &BadgePayload,
) -> Result<PathBuf, AutterError> {
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.workdir()?.join(path)
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(payload)?;
    text.push('\n');
    std::fs::write(&target, text)?;
    Ok(target)
}

fn window_description(share: &RepoShare) -> String {
    match &share.since {
        Some(since) => format!("since {since}"),
        None => "all time".to_string(),
    }
}

/// `autter badge [--since <v> | --all-time] [--json] [--write [path]]`.
pub fn handle_badge(args: &[String]) {
    let pre = match arg_parser::pre_parse(args, ScanMode::Full, false) {
        Ok(pre) => pre,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    };
    if pre.flags.help {
        arg_parser::print_command_help("badge");
        return;
    }
    arg_parser::merge_global_flags(&pre.flags);
    let options = match parse_options(&pre.rest) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("error: {e}");
            arg_parser::print_command_help("badge");
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    };
    if let Err(e) = run(options, arg_parser::json()) {
        eprintln!("Badge failed: {e}");
        std::process::exit(crate::commands::EXIT_RUNTIME_ERROR);
    }
}

fn run(options: Options, json_output: bool) -> Result<(), AutterError> {
    let repo = find_repository(&Vec::<String>::new())?;
    let window = options.window.unwrap_or_else(Window::default_window);
    let patterns = effective_ignore_patterns(&repo, &options.ignore_patterns, &[]);
    let share = repo_share(&repo, &window, &patterns)?;
    let payload = badge_payload(&share);

    let written = match &options.write {
        Some(path) => Some(write_payload(&repo, path, &payload)?),
        None => None,
    };

    if json_output {
        println!("{}", serde_json::to_string(&payload)?);
        return Ok(());
    }

    println!(
        "{LABEL}: {}  ({} AI-accepted of {} added lines)",
        badge_message(&share),
        share.ai_accepted,
        share.added_lines
    );
    println!(
        "Window: {} non-merge commits {}, {} with authorship notes",
        share.commits,
        window_description(&share),
        share.commits_with_authorship
    );
    if share.capped {
        println!(
            "Note: only the newest {} commits were counted.",
            crate::authorship::repo_share::MAX_COMMITS
        );
    }
    if let Some(target) = &written {
        println!("Wrote {}", target.display());
    }

    // The written file's path inside the repository, as shields.io would fetch it.
    let written_in_repo = written.as_deref().and_then(|target| {
        let workdir = repo.workdir().ok()?;
        let relative = target.strip_prefix(&workdir).ok()?;
        Some(crate::utils::normalize_to_posix(
            &relative.to_string_lossy(),
        ))
    });
    let (snippet, notes) = snippet_for(&repo, &share, written_in_repo.as_deref());
    let copied = std::io::IsTerminal::is_terminal(&std::io::stderr())
        && crate::ui::clipboard::request_copy(&snippet);
    println!();
    println!(
        "README snippet{}:",
        if copied { " (copied to clipboard)" } else { "" }
    );
    println!();
    println!("  {snippet}");
    for note in notes {
        println!();
        println!("{note}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(ai_accepted: u64, added_lines: u64) -> RepoShare {
        RepoShare {
            ai_percent: crate::authorship::repo_share::percent(ai_accepted, added_lines),
            ai_accepted,
            added_lines,
            commits: 3,
            commits_with_authorship: 2,
            since: Some("2025-08-05".to_string()),
            capped: false,
        }
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn colors_step_from_grey_to_teal() {
        assert_eq!(badge_color(&share(0, 0)), "lightgrey");
        assert_eq!(badge_color(&share(1, 10)), "74c0fc");
        assert_eq!(badge_color(&share(25, 100)), "007ec6");
        assert_eq!(badge_color(&share(50, 100)), "5b5bd6");
        assert_eq!(badge_color(&share(75, 100)), "2ee6c5");
        assert_eq!(badge_color(&share(100, 100)), "2ee6c5");
    }

    #[test]
    fn message_rounds_to_whole_percent() {
        assert_eq!(badge_message(&share(724, 1000)), "72%");
        assert_eq!(badge_message(&share(725, 1000)), "73%");
        assert_eq!(badge_message(&share(0, 0)), "no data");
    }

    #[test]
    fn json_is_a_shields_endpoint_payload() {
        let value = serde_json::to_value(badge_payload(&share(72, 100))).unwrap();
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["label"], LABEL);
        assert_eq!(value["message"], "72%");
        assert_eq!(value["color"], "5b5bd6");
        assert_eq!(value["ai_percent"], 72.0);
        assert_eq!(value["since"], "2025-08-05");
    }

    #[test]
    fn static_badge_escapes_the_label_dash() {
        assert_eq!(
            static_badge_url(&share(72, 100)),
            "https://img.shields.io/badge/AI--written-72%25-5b5bd6"
        );
        assert_eq!(
            static_badge_url(&share(0, 0)),
            "https://img.shields.io/badge/AI--written-no_data-lightgrey"
        );
    }

    #[test]
    fn endpoint_badge_encodes_the_raw_url() {
        assert_eq!(
            endpoint_badge_url("https://raw.githubusercontent.com/o/r/main/.autter/badge.json"),
            "https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fo%2Fr%2Fmain%2F.autter%2Fbadge.json"
        );
    }

    #[test]
    fn github_raw_url_only_for_github_remotes() {
        assert_eq!(
            github_raw_url("https://github.com/o/r", "main", ".autter/badge.json").as_deref(),
            Some("https://raw.githubusercontent.com/o/r/main/.autter/badge.json")
        );
        assert_eq!(
            github_raw_url("https://gitlab.com/o/r", "main", ".autter/badge.json"),
            None
        );
    }

    #[test]
    fn parses_window_and_write_flags() {
        assert_eq!(parse_options(&args(&[])).unwrap(), Options::default());
        assert_eq!(
            parse_options(&args(&["--since", "30 days ago"]))
                .unwrap()
                .window,
            Some(Window::Since("30 days ago".into()))
        );
        assert_eq!(
            parse_options(&args(&["--since=2025-01-01", "--write"])).unwrap(),
            Options {
                window: Some(Window::Since("2025-01-01".into())),
                write: Some(PathBuf::from(DEFAULT_WRITE_PATH)),
                ignore_patterns: vec![],
            }
        );
        assert_eq!(
            parse_options(&args(&["--write", "docs/badge.json", "--all-time"])).unwrap(),
            Options {
                window: Some(Window::AllTime),
                write: Some(PathBuf::from("docs/badge.json")),
                ignore_patterns: vec![],
            }
        );
        assert_eq!(
            parse_options(&args(&["--ignore", "*.lock", "vendor/**"]))
                .unwrap()
                .ignore_patterns,
            vec!["*.lock".to_string(), "vendor/**".to_string()]
        );
        assert!(parse_options(&args(&["--since", "1 day ago", "--all-time"])).is_err());
        assert!(parse_options(&args(&["--since"])).is_err());
        assert!(parse_options(&args(&["--ignore"])).is_err());
        assert!(parse_options(&args(&["--bogus"])).is_err());
    }
}
