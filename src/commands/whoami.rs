use std::io::IsTerminal;

use crate::api::client::ApiContext;
use crate::auth::notice::CloudSyncState;
use crate::auth::state::AuthStatus;
use crate::auth::{AuthState, collect_auth_status, format_unix_timestamp};
use crate::config;
use crate::ui::{self, TermCaps};

pub fn handle_whoami(args: &[String]) {
    if args
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "help")
    {
        print_help();
        std::process::exit(0);
    }

    if !args.is_empty() {
        eprintln!("Error: unknown whoami argument(s): {}", args.join(" "));
        print_help();
        std::process::exit(crate::commands::EXIT_USAGE_ERROR);
    }

    // Use Config::fresh() to support runtime config updates (daemon mode)
    let api_base_url = config::Config::fresh().api_base_url().to_string();
    let auth = collect_auth_status();
    let api_ctx = ApiContext::new(None);

    // A person at a terminal gets a card; anything reading stdout keeps the
    // `Key: value` lines below.
    if std::io::stdout().is_terminal() && ui::caps().tty {
        let report = crate::auth::notice::collect_cloud_sync_status();
        let view = WhoamiView {
            auth: &auth,
            api_base_url: &api_base_url,
            api_key: api_ctx.api_key.as_deref().map(mask_api_key),
            sync: SyncSummary {
                state: report.state,
                pending: report.pending.total,
                held: report.held.total(),
            },
            now: chrono::Utc::now().timestamp(),
        };
        print!("{}", render_card(&view, &ui::caps()));
        if !matches!(auth.state, AuthState::LoggedIn) && api_ctx.api_key.is_none() {
            std::process::exit(1);
        }
        return;
    }

    println!("API Base URL: {}", api_base_url);
    println!("Credential backend: {}", auth.backend);

    if let Some(api_key) = &api_ctx.api_key {
        let masked = mask_api_key(api_key);
        println!("API key: {}", masked);
    }

    let mut auth_failed = false;
    match &auth.state {
        AuthState::LoggedOut => {
            println!("Auth state: logged out");
            if api_ctx.api_key.is_none() {
                auth_failed = true;
            }
        }
        AuthState::LoggedIn => {
            println!("Auth state: logged in");
        }
        AuthState::SyncBlocked => {
            println!("Auth state: cloud sync blocked (stored session could not authenticate)");
            println!("Fix: run `autter login`, then `autter bg restart`");
            if api_ctx.api_key.is_none() {
                auth_failed = true;
            }
        }
        AuthState::RefreshExpired => {
            println!("Auth state: credentials expired (refresh token expired)");
            if api_ctx.api_key.is_none() {
                auth_failed = true;
            }
        }
        AuthState::LoginRefused(reason) => {
            println!("Auth state: sign-in refused by the server ({})", reason);
            println!("Fix: run `autter login`");
            if api_ctx.api_key.is_none() {
                auth_failed = true;
            }
        }
        AuthState::Error(err) => {
            println!("Auth state: error ({})", err);
            if api_ctx.api_key.is_none() {
                auth_failed = true;
            }
        }
    }

    if let Some(expires_at) = auth.access_token_expires_at {
        println!(
            "Access token expires at: {}",
            format_unix_timestamp(expires_at)
        );
    }
    if let Some(expires_at) = auth.refresh_token_expires_at {
        println!(
            "Refresh token expires at: {}",
            format_unix_timestamp(expires_at)
        );
    }

    println!(
        "User ID: {}",
        auth.user_id.unwrap_or_else(|| "<unavailable>".to_string())
    );
    println!(
        "Email: {}",
        auth.email.unwrap_or_else(|| "<unavailable>".to_string())
    );
    println!(
        "Name: {}",
        auth.name.unwrap_or_else(|| "<unavailable>".to_string())
    );
    println!(
        "Personal org ID: {}",
        auth.personal_org_id
            .unwrap_or_else(|| "<unavailable>".to_string())
    );
    if auth.orgs.is_empty() {
        println!("Organizations: <none>");
    } else {
        println!("Organizations:");
        for org in auth.orgs {
            let org_id = org.org_id.unwrap_or_else(|| "<unknown-id>".to_string());
            let org_slug = org.org_slug.unwrap_or_else(|| "<unknown-slug>".to_string());
            let org_name = org.org_name.unwrap_or_else(|| "<unknown-name>".to_string());
            let role = org.role.unwrap_or_else(|| "<unknown-role>".to_string());
            println!("  - {} ({}) [{}] role={}", org_slug, org_name, org_id, role);
        }
    }

    if let Some(line) = crate::auth::notice::format_cloud_sync_status_line() {
        println!("{line}");
    } else {
        let pending = crate::auth::notice::pending_sync_counts();
        if pending.total() > 0 {
            println!(
                "Pending cloud sync: {} (upload in progress)",
                pending.summary()
            );
        } else {
            println!("Pending cloud sync: none");
        }
    }

    if auth_failed {
        std::process::exit(1);
    }
}

struct SyncSummary {
    state: CloudSyncState,
    pending: i64,
    held: i64,
}

struct WhoamiView<'a> {
    auth: &'a AuthStatus,
    api_base_url: &'a str,
    /// Already masked.
    api_key: Option<String>,
    sync: SyncSummary,
    /// Unix seconds, for relative expiry times.
    now: i64,
}

/// The terminal card: the brand header (account, sign-in state, credential
/// backend) and one row each for organizations, cloud sync and the API.
fn render_card(view: &WhoamiView, caps: &TermCaps) -> String {
    let auth = view.auth;
    let who = account_label(auth);
    let fix = |text: &str| format!("run {}", caps.paint("36", text));
    let (title, state) = match &auth.state {
        AuthState::LoggedIn => {
            let valid = auth
                .refresh_token_expires_at
                .map(|at| format!(" \u{00B7} session valid {}", relative_until(at - view.now)))
                .unwrap_or_default();
            (who, format!("Signed in{valid}"))
        }
        AuthState::LoggedOut if view.api_key.is_some() => {
            ("Signed in with an API key".to_string(), String::new())
        }
        AuthState::LoggedOut => (
            "Not signed in".to_string(),
            format!("Sign in: {}", fix("autter login")),
        ),
        AuthState::SyncBlocked => (
            who,
            format!(
                "Cloud sync blocked \u{00B7} {}, then {}",
                fix("autter login"),
                caps.paint("36", "autter bg restart")
            ),
        ),
        AuthState::RefreshExpired => (
            who,
            format!("Session expired \u{00B7} {}", fix("autter login")),
        ),
        AuthState::LoginRefused(reason) => (
            who,
            format!(
                "Sign-in refused ({reason}) \u{00B7} {}",
                fix("autter login")
            ),
        ),
        AuthState::Error(err) => ("Couldn't read your sign-in".to_string(), err.clone()),
    };
    let mut out =
        ui::Header::new(title, "", state, format!("Credentials in {}", auth.backend)).render(caps);
    out.push('\n');

    let row = |label: &str, value: &str| {
        format!("  {}{value}\n", caps.paint("2", &format!("{label:<15}")))
    };
    let orgs = if auth.orgs.is_empty() {
        caps.paint("2", "none")
    } else {
        auth.orgs
            .iter()
            .map(|org| {
                let name = org
                    .org_slug
                    .as_deref()
                    .or(org.org_name.as_deref())
                    .unwrap_or("unnamed");
                match org.role.as_deref() {
                    Some(role) => format!("{name} {}", caps.paint("2", &format!("({role})"))),
                    None => name.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join(" \u{00B7} ")
    };
    out.push_str(&row("Organizations", &orgs));
    out.push_str(&row("Cloud sync", &sync_label(&view.sync, caps)));
    let api = match &view.api_key {
        Some(key) => format!("{} \u{00B7} key {key}", view.api_base_url),
        None => view.api_base_url.to_string(),
    };
    out.push_str(&row("API", &api));
    if let Some(user_id) = &auth.user_id {
        out.push_str(&row("User ID", &caps.paint("2", user_id)));
    }
    if view.sync.held > 0 {
        out.push('\n');
        out.push_str(&format!(
            "  {} records from before you connected are held and won't upload.\n  Upload: {} \u{00B7} Delete: {}\n",
            view.sync.held,
            caps.paint("36", crate::upload_hold::IMPORT_COMMAND),
            caps.paint("36", crate::upload_hold::PURGE_COMMAND)
        ));
    }
    out
}

fn account_label(auth: &AuthStatus) -> String {
    let name = auth.name.as_deref().filter(|s| !s.is_empty());
    let email = auth.email.as_deref().filter(|s| !s.is_empty());
    match (name, email) {
        (Some(name), Some(email)) => format!("{name} ({email})"),
        (Some(who), None) | (None, Some(who)) => who.to_string(),
        (None, None) => "Unknown account".to_string(),
    }
}

fn sync_label(sync: &SyncSummary, caps: &TermCaps) -> String {
    let glyphs = caps.glyphs();
    let fix = |text: &str| caps.paint("36", text);
    match sync.state {
        CloudSyncState::Healthy => format!("{} up to date", caps.paint("32", glyphs.ok)),
        CloudSyncState::Draining => format!("{} records uploading", sync.pending),
        CloudSyncState::Disabled => caps.paint("2", "off \u{00B7} local mode, nothing uploads"),
        CloudSyncState::AuthBlocked => format!(
            "{} blocked \u{00B7} run {}",
            caps.paint("33", glyphs.warn),
            fix("autter login")
        ),
        CloudSyncState::UploadFailing => format!(
            "{} upload failing \u{00B7} run {}",
            caps.paint("31", glyphs.fail),
            fix("autter doctor")
        ),
        CloudSyncState::DaemonNotRunning => format!(
            "{} background service stopped \u{00B7} run {}",
            caps.paint("33", glyphs.warn),
            fix("autter bg restart")
        ),
        CloudSyncState::StatusUnavailable => {
            format!("status unavailable \u{00B7} run {}", fix("autter doctor"))
        }
    }
}

/// "for 6 days", "for 3 hours", "for 12 minutes", or "expired".
fn relative_until(seconds: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("for 1 {unit}")
        } else {
            format!("for {n} {unit}s")
        }
    };
    if seconds <= 0 {
        "expired".to_string()
    } else if seconds < HOUR {
        plural((seconds / MINUTE).max(1), "minute")
    } else if seconds < DAY {
        plural(seconds / HOUR, "hour")
    } else {
        plural(seconds / DAY, "day")
    }
}

fn mask_api_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{}...{}", prefix, suffix)
}

fn print_help() {
    eprintln!("autter whoami - Show current auth state and identity");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  autter whoami");
    eprintln!("  autter whoami --help");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::identity::TokenOrg;
    use crate::ui::theme::{Charset, ColorLevel};

    fn caps() -> TermCaps {
        TermCaps {
            tty: true,
            color: ColorLevel::None,
            charset: Charset::Unicode,
            width: 100,
        }
    }

    fn signed_in() -> AuthStatus {
        AuthStatus {
            backend: "file".to_string(),
            state: AuthState::LoggedIn,
            access_token_expires_at: Some(1_000_000 + 3600),
            refresh_token_expires_at: Some(1_000_000 + 89 * 86_400 + 10),
            user_id: Some("usr_123".to_string()),
            email: Some("you@company.com".to_string()),
            name: Some("Sam".to_string()),
            personal_org_id: None,
            orgs: vec![
                TokenOrg {
                    org_id: Some("org_1".to_string()),
                    org_name: Some("Acme".to_string()),
                    org_slug: Some("acme".to_string()),
                    role: Some("admin".to_string()),
                },
                TokenOrg {
                    org_id: None,
                    org_name: Some("Personal".to_string()),
                    org_slug: None,
                    role: None,
                },
            ],
        }
    }

    fn view(auth: &AuthStatus, state: CloudSyncState, held: i64) -> WhoamiView<'_> {
        WhoamiView {
            auth,
            api_base_url: "https://api.autter.dev",
            api_key: None,
            sync: SyncSummary {
                state,
                pending: 3,
                held,
            },
            now: 1_000_000,
        }
    }

    #[test]
    fn signed_in_card() {
        let auth = signed_in();
        insta::assert_snapshot!(render_card(
            &view(&auth, CloudSyncState::Healthy, 0),
            &caps()
        ));
    }

    #[test]
    fn signed_out_card_points_at_login() {
        let auth = AuthStatus {
            state: AuthState::LoggedOut,
            email: None,
            name: None,
            user_id: None,
            orgs: Vec::new(),
            ..signed_in()
        };
        let text = render_card(&view(&auth, CloudSyncState::Disabled, 2), &caps());
        assert!(text.contains("Not signed in"), "{text}");
        assert!(text.contains("Sign in: run autter login"), "{text}");
        assert!(text.contains("off · local mode"), "{text}");
        assert!(
            text.contains("2 records from before you connected"),
            "{text}"
        );
        assert!(!text.contains("User ID"), "{text}");
    }

    #[test]
    fn relative_expiry() {
        assert_eq!(relative_until(0), "expired");
        assert_eq!(relative_until(30), "for 1 minute");
        assert_eq!(relative_until(3 * 3600 + 5), "for 3 hours");
        assert_eq!(relative_until(86_400), "for 1 day");
        assert_eq!(relative_until(89 * 86_400 + 10), "for 89 days");
    }
}
