//! `autter dash`: open the Autter page for where you are.
//!
//! Inside a git repository that is connected to your organization this opens
//! that repository's page; otherwise the organization dashboard; when you are
//! not signed in, the app's dashboard. The repository is matched by its
//! default remote (`owner/name`) against the organization's connected
//! repositories, read with a read-only status request that does not start an
//! index refresh.

use crate::api::client::ApiContext;
use crate::commands::arg_parser::{self, ScanMode};
use crate::commands::ask::{component, org_api_context};
use crate::commands::output::{ErrorKind, fail};
use serde::Serialize;
use serde_json::Value;

/// How long `dash` waits for the repository lookup before falling back to the
/// organization dashboard.
const LOOKUP_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Default, PartialEq, Eq)]
struct DashOptions {
    organization: Option<String>,
    repository: Option<String>,
    no_open: bool,
}

fn parse_options(args: &[String]) -> Result<DashOptions, String> {
    let mut options = DashOptions::default();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--no-open" => {
                options.no_open = true;
                index += 1;
            }
            "--org" | "--repo" => {
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.starts_with('-') && !value.trim().is_empty())
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                if flag == "--org" {
                    options.organization = Some(value.clone());
                } else {
                    options.repository = Some(value.clone());
                }
                index += 2;
            }
            _ => return Err(format!("unknown option for dashboard: {flag}")),
        }
    }
    Ok(options)
}

/// Which page `dash` opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DashTarget {
    /// The repository's page in the organization.
    Repo,
    /// The organization dashboard.
    Org,
    /// The app's dashboard (organization picker).
    Home,
}

/// Why `dash` did not open the repository page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FallbackReason {
    NotInGitRepo,
    NoRemote,
    NotSignedIn,
    RepoNotConnected,
    LookupFailed,
}

/// A connected repository: its short `name` (the URL segment) and `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectedRepo {
    pub name: String,
    pub full_name: String,
}

/// Result of looking the candidate up in the organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepoLookup {
    Found(ConnectedRepo),
    NotConnected,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DashResolution {
    pub url: String,
    pub target: DashTarget,
    pub organization: Option<String>,
    pub repository: Option<String>,
    pub repository_full_name: Option<String>,
    pub fallback_reason: Option<FallbackReason>,
    /// The `owner/name` (or name) that was looked up, for the fallback message.
    #[serde(skip)]
    pub candidate: Option<String>,
}

/// `web/<segments…>`, with every segment percent-encoded and no double slash.
fn web_url(web_app_base: &str, segments: &[&str]) -> String {
    let base = web_app_base.trim_end_matches('/');
    if let Ok(mut url) = url::Url::parse(base) {
        let pushed = match url.path_segments_mut() {
            Ok(mut path) => {
                path.pop_if_empty();
                for segment in segments {
                    path.push(segment);
                }
                true
            }
            Err(()) => false,
        };
        if pushed {
            return url.to_string();
        }
    }
    // Not a URL we can parse (should not happen): join encoded segments.
    let mut out = base.to_string();
    for segment in segments {
        out.push('/');
        out.push_str(&component(segment));
    }
    out
}

/// Pick the page to open. Pure: every input that needs I/O is resolved first.
///
/// * `organization`: the org slug, or `None` when not signed in.
/// * `candidate`: the repository to look for (`--repo`, else the current
///   repo's `owner/name`), or why there is none.
/// * `lookup`: matches the candidate in the organization; only called when
///   there are both an organization and a candidate.
pub(crate) fn resolve_dash_target(
    web_app_base: &str,
    organization: Option<&str>,
    candidate: Result<&str, FallbackReason>,
    lookup: impl FnOnce(&str, &str) -> RepoLookup,
) -> DashResolution {
    let candidate_name = candidate.ok().map(str::to_owned);
    let Some(org) = organization else {
        return DashResolution {
            url: web_url(web_app_base, &["dashboard"]),
            target: DashTarget::Home,
            organization: None,
            repository: None,
            repository_full_name: None,
            fallback_reason: Some(FallbackReason::NotSignedIn),
            candidate: candidate_name,
        };
    };
    let org_page = |reason: FallbackReason| DashResolution {
        url: web_url(web_app_base, &[org]),
        target: DashTarget::Org,
        organization: Some(org.to_owned()),
        repository: None,
        repository_full_name: None,
        fallback_reason: Some(reason),
        candidate: candidate_name.clone(),
    };
    let wanted = match candidate {
        Ok(wanted) => wanted,
        Err(reason) => return org_page(reason),
    };
    match lookup(org, wanted) {
        RepoLookup::Found(repo) => DashResolution {
            url: web_url(web_app_base, &[org, "repositories", &repo.name]),
            target: DashTarget::Repo,
            organization: Some(org.to_owned()),
            repository: Some(repo.name),
            repository_full_name: Some(repo.full_name),
            fallback_reason: None,
            candidate: candidate_name.clone(),
        },
        RepoLookup::NotConnected => org_page(FallbackReason::RepoNotConnected),
        RepoLookup::Failed => org_page(FallbackReason::LookupFailed),
    }
}

/// `owner/name` from a normalized `https://host/owner/name` remote URL.
pub(crate) fn owner_and_name(normalized_remote: &str) -> Option<String> {
    let url = url::Url::parse(normalized_remote).ok()?;
    let path = url.path().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    path.contains('/').then(|| path.to_owned())
}

/// Find `wanted` among the connected repositories: `owner/name` matches the
/// full name, a bare name matches the short name; both ignore case.
pub(crate) fn match_repository(wanted: &str, repos: &[ConnectedRepo]) -> Option<ConnectedRepo> {
    let wanted = wanted.trim().trim_matches('/');
    let found = if wanted.contains('/') {
        repos
            .iter()
            .find(|repo| repo.full_name.eq_ignore_ascii_case(wanted))
    } else {
        repos
            .iter()
            .find(|repo| repo.name.eq_ignore_ascii_case(wanted))
    };
    found.cloned()
}

/// Parse the `repositories` list of an `ask/status` response.
pub(crate) fn connected_repositories(status: &Value) -> Option<Vec<ConnectedRepo>> {
    let list = status.get("repositories")?.as_array()?;
    Some(
        list.iter()
            .filter_map(|repo| {
                Some(ConnectedRepo {
                    name: repo.get("name")?.as_str()?.to_owned(),
                    full_name: repo.get("fullName")?.as_str()?.to_owned(),
                })
            })
            .collect(),
    )
}

/// The organization's connected repositories, read without side effects
/// (`warm=0`: no index refresh). `None` on any failure.
pub(crate) fn fetch_connected_repositories(
    context: &ApiContext,
    slug: &str,
) -> Option<Vec<ConnectedRepo>> {
    let response = context
        .get(&format!(
            "/api/cli/orgs/{}/ask/status?warm=0",
            component(slug)
        ))
        .ok()?;
    if response.status_code != 200 {
        return None;
    }
    let value: Value = serde_json::from_str(response.as_str().ok()?).ok()?;
    connected_repositories(&value)
}

/// The current repository's `owner/name`, or why there is none.
pub(crate) fn current_repository_candidate() -> Result<String, FallbackReason> {
    let repo = crate::git::repository::find_repository(&Vec::<String>::new())
        .map_err(|_| FallbackReason::NotInGitRepo)?;
    crate::authorship::post_commit::current_repo_url(&repo)
        .as_deref()
        .and_then(owner_and_name)
        .ok_or(FallbackReason::NoRemote)
}

fn fallback_message(resolution: &DashResolution) -> Option<String> {
    let org = resolution
        .organization
        .as_deref()
        .unwrap_or("your organization");
    let candidate = resolution.candidate.as_deref().unwrap_or("This repository");
    Some(match resolution.fallback_reason? {
        FallbackReason::NotSignedIn => "Not signed in; opening the Autter dashboard. Run `autter login` to open your repository's page.".to_string(),
        FallbackReason::NotInGitRepo => {
            "Not in a git repository; opening the organization dashboard.".to_string()
        }
        FallbackReason::NoRemote => {
            "This repository has no remote; opening the organization dashboard.".to_string()
        }
        FallbackReason::RepoNotConnected => {
            format!("{candidate} is not connected to {org}; opening the organization dashboard.")
        }
        FallbackReason::LookupFailed => {
            format!("Could not look up {candidate} in {org}; opening the organization dashboard.")
        }
    })
}

#[derive(Serialize)]
struct DashReport<'a> {
    #[serde(flatten)]
    resolution: &'a DashResolution,
    opened: bool,
}

/// Handle `autter dash` (also `dashboard`, `personal-dashboard`).
pub fn handle_personal_dashboard(args: &[String]) {
    let parsed = arg_parser::pre_parse(args, ScanMode::Full, true)
        .unwrap_or_else(|error| fail(ErrorKind::Usage, format!("error: {error}")));
    arg_parser::merge_global_flags(&parsed.flags);
    if parsed.flags.help {
        arg_parser::print_command_help("dashboard");
        return;
    }
    if let Some(directory) = parsed.flags.change_dir
        && let Err(error) = std::env::set_current_dir(directory)
    {
        fail(ErrorKind::Usage, format!("error: {error}"));
    }
    let options = parse_options(&parsed.rest)
        .unwrap_or_else(|error| fail(ErrorKind::Usage, format!("error: {error}")));

    let web = crate::commands::login::web_app_url();
    let signed_in = ApiContext::new(None).auth_token.is_some();
    // An explicit --org that the token cannot use is an error; without --org,
    // a token with no usable organization just means "not signed in".
    let org_context = if signed_in {
        match org_api_context(options.organization.as_deref(), LOOKUP_TIMEOUT_SECS) {
            Ok(found) => Some(found),
            Err(error) if options.organization.is_some() => {
                fail(ErrorKind::Usage, format!("error: {error}"))
            }
            Err(_) => None,
        }
    } else if options.organization.is_some() {
        fail(
            ErrorKind::NotSignedIn,
            "error: Sign in with `autter login` to open an organization.",
        )
    } else {
        None
    };

    let candidate = match options.repository.clone() {
        Some(repo) => Ok(repo),
        None => current_repository_candidate(),
    };
    let resolution = resolve_dash_target(
        &web,
        org_context.as_ref().map(|(_, slug)| slug.as_str()),
        candidate.as_deref().map_err(|reason| *reason),
        |slug, wanted| {
            let Some((context, _)) = org_context.as_ref() else {
                return RepoLookup::Failed;
            };
            match fetch_connected_repositories(context, slug) {
                Some(repos) => match match_repository(wanted, &repos) {
                    Some(repo) => RepoLookup::Found(repo),
                    None => RepoLookup::NotConnected,
                },
                None => RepoLookup::Failed,
            }
        },
    );

    let json = arg_parser::json();
    let mut opened = false;
    if options.no_open {
        if !json {
            println!("{}", resolution.url);
        }
    } else {
        if !json {
            eprintln!("Opening dashboard: {}", resolution.url);
        }
        opened = open_browser(&resolution.url).is_ok();
        if !opened {
            eprintln!("Could not open browser automatically.");
            eprintln!("Visit this URL in your browser:");
            eprintln!("  {}", resolution.url);
        }
    }
    if let Some(note) = fallback_message(&resolution)
        && !arg_parser::quiet()
        && !json
    {
        eprintln!("{note}");
    }
    if json {
        let report = DashReport {
            resolution: &resolution,
            opened,
        };
        match serde_json::to_string(&report) {
            Ok(out) => println!("{out}"),
            Err(error) => fail(ErrorKind::Runtime, format!("error: {error}")),
        }
    }
}

/// Attempt to open a URL in the system's default browser
pub(crate) fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEB: &str = "https://app.autter.dev";

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn repos() -> Vec<ConnectedRepo> {
        vec![
            ConnectedRepo {
                name: "api".into(),
                full_name: "Acme/api".into(),
            },
            ConnectedRepo {
                name: "web".into(),
                full_name: "acme/web".into(),
            },
        ]
    }

    #[test]
    fn dash_parses_flags() {
        assert_eq!(parse_options(&[]).unwrap(), DashOptions::default());
        assert_eq!(
            parse_options(&args(&["--org", "acme", "--repo", "acme/api", "--no-open"])).unwrap(),
            DashOptions {
                organization: Some("acme".into()),
                repository: Some("acme/api".into()),
                no_open: true,
            }
        );
        assert!(parse_options(&args(&["--org"])).is_err());
        assert!(parse_options(&args(&["--repo", "--no-open"])).is_err());
        assert!(parse_options(&args(&["--bogus"])).is_err());
        assert!(parse_options(&args(&["extra"])).is_err());
    }

    #[test]
    fn dash_opens_the_repository_page_when_connected() {
        let out = resolve_dash_target(WEB, Some("acme"), Ok("acme/api"), |org, wanted| {
            assert_eq!((org, wanted), ("acme", "acme/api"));
            RepoLookup::Found(ConnectedRepo {
                name: "api".into(),
                full_name: "acme/api".into(),
            })
        });
        assert_eq!(out.url, "https://app.autter.dev/acme/repositories/api");
        assert_eq!(out.target, DashTarget::Repo);
        assert_eq!(out.repository.as_deref(), Some("api"));
        assert_eq!(out.repository_full_name.as_deref(), Some("acme/api"));
        assert_eq!(out.fallback_reason, None);
    }

    #[test]
    fn dash_falls_back_to_the_org_with_a_reason() {
        for reason in [FallbackReason::NotInGitRepo, FallbackReason::NoRemote] {
            let out = resolve_dash_target(WEB, Some("acme"), Err(reason), |_, _| {
                panic!("no lookup without a candidate")
            });
            assert_eq!(out.url, "https://app.autter.dev/acme");
            assert_eq!(out.target, DashTarget::Org);
            assert_eq!(out.fallback_reason, Some(reason));
        }
        for (lookup, reason) in [
            (RepoLookup::NotConnected, FallbackReason::RepoNotConnected),
            (RepoLookup::Failed, FallbackReason::LookupFailed),
        ] {
            let out = resolve_dash_target(WEB, Some("acme"), Ok("acme/other"), |_, _| lookup);
            assert_eq!(out.url, "https://app.autter.dev/acme");
            assert_eq!(out.target, DashTarget::Org);
            assert_eq!(out.fallback_reason, Some(reason));
            assert_eq!(out.candidate.as_deref(), Some("acme/other"));
        }
    }

    #[test]
    fn dash_opens_the_app_dashboard_when_signed_out() {
        let out = resolve_dash_target("https://app.autter.dev/", None, Ok("acme/api"), |_, _| {
            panic!("no lookup when signed out")
        });
        assert_eq!(out.url, "https://app.autter.dev/dashboard");
        assert_eq!(out.target, DashTarget::Home);
        assert_eq!(out.fallback_reason, Some(FallbackReason::NotSignedIn));
    }

    #[test]
    fn dash_urls_encode_segments_and_keep_staging_hosts() {
        assert_eq!(
            web_url("https://app.autter.dev/", &["acme"]),
            "https://app.autter.dev/acme"
        );
        assert_eq!(
            web_url(
                "https://test-app.autter.dev",
                &["acme", "repositories", "web app"]
            ),
            "https://test-app.autter.dev/acme/repositories/web%20app"
        );
        assert_eq!(
            web_url(WEB, &["a/b?c#d"]),
            "https://app.autter.dev/a%2Fb%3Fc%23d"
        );
    }

    #[test]
    fn dash_extracts_owner_and_name_from_remotes() {
        for remote in [
            "https://github.com/Acme/api.git",
            "git@github.com:Acme/api.git",
            "ssh://git@github.com/Acme/api",
        ] {
            let normalized = crate::repo_url::normalize_repo_url(remote).unwrap();
            assert_eq!(
                owner_and_name(&normalized).as_deref(),
                Some("Acme/api"),
                "{remote}"
            );
        }
        assert_eq!(owner_and_name("https://github.com/"), None);
        assert_eq!(owner_and_name("https://github.com/solo"), None);
    }

    #[test]
    fn dash_matches_repositories_ignoring_case() {
        let name = |wanted: &str| match_repository(wanted, &repos()).map(|repo| repo.name);
        assert_eq!(name("acme/API").as_deref(), Some("api"));
        assert_eq!(name("API").as_deref(), Some("api"));
        assert_eq!(name("other/api"), None);
        assert_eq!(name("missing"), None);
    }

    #[test]
    fn dash_reads_the_status_repository_list() {
        let status = serde_json::json!({
            "repositories": [{ "name": "api", "fullName": "acme/api" }, { "name": 3 }],
            "repository": null,
        });
        assert_eq!(
            connected_repositories(&status),
            Some(vec![ConnectedRepo {
                name: "api".into(),
                full_name: "acme/api".into()
            }])
        );
        assert_eq!(
            connected_repositories(&serde_json::json!({ "error": "x" })),
            None
        );
    }

    #[test]
    fn dash_json_report_shape() {
        let resolution = resolve_dash_target(WEB, Some("acme"), Ok("acme/other"), |_, _| {
            RepoLookup::NotConnected
        });
        let json = serde_json::to_value(DashReport {
            resolution: &resolution,
            opened: false,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "url": "https://app.autter.dev/acme",
                "target": "org",
                "organization": "acme",
                "repository": null,
                "repository_full_name": null,
                "fallback_reason": "repo_not_connected",
                "opened": false,
            })
        );
        assert_eq!(
            fallback_message(&resolution).as_deref(),
            Some("acme/other is not connected to acme; opening the organization dashboard.")
        );
    }
}
