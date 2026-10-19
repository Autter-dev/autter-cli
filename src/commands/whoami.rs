use crate::api::client::ApiContext;
use crate::auth::{AuthState, collect_auth_status, format_unix_timestamp};
use crate::config;

pub fn handle_whoami(args: &[String]) {
    if args
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "help")
    {
        print_help();
        std::process::exit(0);
    }

    let short = args.iter().any(|arg| arg.as_str() == "--short" || arg.as_str() == "-s");
    let unknown: Vec<&str> = args
        .iter()
        .map(|arg| arg.as_str())
        .filter(|arg| *arg != "--short" && *arg != "-s")
        .collect();
    if !unknown.is_empty() {
        eprintln!("Error: unknown whoami argument(s): {}", unknown.join(" "));
        print_help();
        std::process::exit(crate::commands::EXIT_USAGE_ERROR);
    }

    if short {
        let auth = collect_auth_status();
        let has_api_key = ApiContext::new(None).api_key.is_some();
        let org_slug = auth.orgs.first().and_then(|org| org.org_slug.as_deref());
        println!("{}", short_identity(&auth.state, auth.email.as_deref(), org_slug));
        if identity_failed(&auth.state, has_api_key) {
            std::process::exit(1);
        }
        return;
    }

    // Use Config::fresh() to support runtime config updates (daemon mode)
    let api_base_url = config::Config::fresh().api_base_url().to_string();
    let auth = collect_auth_status();
    let api_ctx = ApiContext::new(None);

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

fn mask_api_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{}...{}", prefix, suffix)
}

/// The auth state in a few words, for `autter whoami --short`.
fn state_label(state: &AuthState) -> String {
    match state {
        AuthState::LoggedOut => "logged out".to_string(),
        AuthState::LoggedIn => "logged in".to_string(),
        AuthState::SyncBlocked => "cloud sync blocked".to_string(),
        AuthState::RefreshExpired => "credentials expired".to_string(),
        AuthState::Error(err) => format!("error: {}", err),
    }
}

/// One line for scripts and shell prompts: `<email> (<org>) <state>`.
fn short_identity(state: &AuthState, email: Option<&str>, org_slug: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(email) = email {
        parts.push(email.to_string());
    }
    if let Some(org) = org_slug {
        parts.push(format!("({})", org));
    }
    parts.push(state_label(state));
    parts.join(" ")
}

/// Same rule as the full report: a session that is not logged in fails
/// unless an API key is configured.
fn identity_failed(state: &AuthState, has_api_key: bool) -> bool {
    *state != AuthState::LoggedIn && !has_api_key
}

fn print_help() {
    eprintln!("autter whoami - Show current auth state and identity");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  autter whoami");
    eprintln!("  autter whoami --short   One line: <email> (<org>) <state>");
    eprintln!("  autter whoami --help");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_identity_names_the_user_org_and_state() {
        assert_eq!(
            short_identity(&AuthState::LoggedIn, Some("ada@example.com"), Some("acme")),
            "ada@example.com (acme) logged in"
        );
        assert_eq!(
            short_identity(&AuthState::RefreshExpired, Some("ada@example.com"), None),
            "ada@example.com credentials expired"
        );
        assert_eq!(short_identity(&AuthState::LoggedOut, None, None), "logged out");
        assert_eq!(
            short_identity(&AuthState::Error("keychain locked".to_string()), None, None),
            "error: keychain locked"
        );
    }

    #[test]
    fn identity_fails_only_without_a_login_or_an_api_key() {
        assert!(!identity_failed(&AuthState::LoggedIn, false));
        assert!(identity_failed(&AuthState::LoggedOut, false));
        assert!(!identity_failed(&AuthState::LoggedOut, true));
        assert!(identity_failed(&AuthState::SyncBlocked, false));
    }

    #[test]
    fn api_keys_are_masked() {
        assert_eq!(mask_api_key("short"), "*****");
        assert_eq!(mask_api_key("abcd1234efgh5678"), "abcd...5678");
    }
}
