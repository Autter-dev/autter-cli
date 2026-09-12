use crate::auth::{CredentialStore, OAuthClient};

/// Result of running the OAuth device-login flow.
pub enum LoginOutcome {
    /// Valid credentials were already present; no new login was performed.
    AlreadyLoggedIn,
    /// A fresh login completed successfully and credentials were stored.
    LoggedIn,
}

/// Run the OAuth2 device-authorization flow.
///
/// Unlike [`handle_login`], this never calls `std::process::exit`, so it can be
/// reused by other flows (e.g. `autter onboard`). It prints user-facing
/// instructions to stderr and returns the outcome (or an error string).
pub fn run_device_login() -> Result<LoginOutcome, String> {
    let store = CredentialStore::new();

    if let Ok(Some(creds)) = store.load()
        && !creds.is_refresh_token_expired()
    {
        if !crate::auth::notice::sync_auth_blocked_recently() {
            return Ok(LoginOutcome::AlreadyLoggedIn);
        }
        // A sync-blocked stamp can outlive a transient refresh failure. Probe a
        // live access token before forcing the user through a new sign-in.
        let ctx = crate::api::client::ApiContext::new(None);
        if ctx.auth_token.is_some() {
            crate::auth::notice::clear_sync_auth_blocked();
            eprintln!("Stored credentials still work. Resuming cloud sync.");
            resume_cloud_sync_after_login();
            return Ok(LoginOutcome::LoggedIn);
        }
        eprintln!("Stored credentials could not authenticate. Starting a new sign-in...\n");
    }

    let client = OAuthClient::new();

    // Start device flow
    eprintln!("Starting device authorization...\n");

    let auth_response = client
        .start_device_flow()
        .map_err(|e| format!("Failed to start authorization: {}", e))?;

    // Build the display URL
    let display_url = auth_response
        .verification_uri_complete
        .as_ref()
        .unwrap_or(&auth_response.verification_uri);

    // Display instructions
    eprintln!("To authorize this device:");
    eprintln!("  1. Open this URL in your browser:");
    eprintln!("     \x1b[36m{}\x1b[0m", display_url);
    eprintln!();
    eprintln!("  2. Enter this code when prompted:");
    eprintln!("     \x1b[1m{}\x1b[0m", auth_response.user_code);
    eprintln!();

    // Try to open browser automatically
    if open_browser(display_url).is_err() {
        eprintln!("  (Could not open browser automatically)");
        eprintln!();
    }

    // Poll for token, with a spinner so the wait doesn't look like a hang.
    // (indicatif draws on stderr, matching the rest of this flow.)
    let spinner = crate::mdm::spinner::Spinner::new("Waiting for you to authorize in the browser…");
    let creds = match client.poll_for_token(
        &auth_response.device_code,
        auth_response.interval,
        auth_response.expires_in,
    ) {
        Ok(creds) => {
            spinner.stop();
            eprintln!("\x1b[1;32m✓ Device authorized.\x1b[0m");
            creds
        }
        Err(e) => {
            spinner.stop();
            return Err(format!("Authorization failed: {}", e));
        }
    };

    // Store credentials (non-fatal on failure)
    match store.store(&creds) {
        Ok(()) => {
            crate::auth::notice::clear_sync_auth_blocked();
            print_login_success(&creds.access_token);
            resume_cloud_sync_after_login();
        }
        Err(e) => {
            eprintln!("\nWarning: Failed to store credentials: {}", e);
            eprintln!("You may need to log in again next time.");
        }
    }

    Ok(LoginOutcome::LoggedIn)
}

/// Sign in with a Personal Access Token instead of the interactive device flow.
///
/// Validates the token by exchanging it for an access token up-front, so a bad
/// token fails immediately rather than being stored and failing later.
pub fn run_pat_login(token: &str) -> Result<LoginOutcome, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("No token provided. Usage: autter login --token <token>".to_string());
    }

    let creds = OAuthClient::new().exchange_pat(token)?;

    let store = CredentialStore::new();
    store
        .store(&creds)
        .map_err(|e| format!("Failed to store credentials: {}", e))?;
    crate::auth::notice::clear_sync_auth_blocked();

    print_login_success(&creds.access_token);
    resume_cloud_sync_after_login();
    Ok(LoginOutcome::LoggedIn)
}

/// Print "Successfully logged in!" plus the signed-in user and active org, read
/// from the access token's claims (best-effort — falls back gracefully).
fn print_login_success(access_token: &str) {
    use crate::auth::identity::extract_identity_from_access_token;

    eprintln!("Successfully logged in!");
    let identity = extract_identity_from_access_token(access_token);

    if let Some(name) = identity.name.as_deref().filter(|s| !s.is_empty()) {
        match identity.email.as_deref().filter(|s| !s.is_empty()) {
            Some(email) => eprintln!("  Signed in as {} ({})", name, email),
            None => eprintln!("  Signed in as {}", name),
        }
    } else if let Some(email) = identity.email.as_deref().filter(|s| !s.is_empty()) {
        eprintln!("  Signed in as {}", email);
    }

    if let Some(org) = identity.active_org()
        && let Some(org_name) = org.org_name.as_deref().filter(|s| !s.is_empty())
    {
        match org.org_slug.as_deref().filter(|s| !s.is_empty()) {
            Some(slug) => eprintln!("  Organization: {} ({})", org_name, slug),
            None => eprintln!("  Organization: {}", org_name),
        }
    }
}

/// Restart the background service so it picks up fresh credentials and clears
/// any in-memory sync backoff. Skipped in test harnesses, which manage their
/// own daemon lifecycle. Best-effort: a failed restart must not fail login.
fn resume_cloud_sync_after_login() {
    if std::env::var_os("AUTTER_TEST_DB_PATH").is_some() {
        return;
    }

    let pending = crate::auth::notice::pending_sync_counts();
    if pending.total() > 0 {
        eprintln!(
            "  {} queued locally — restarting background service to upload.",
            pending.summary()
        );
    } else {
        eprintln!("  Restarting background service to resume cloud sync.");
    }

    let Ok(daemon_config) = crate::daemon::DaemonConfig::from_env_or_default_paths() else {
        eprintln!("  Warning: could not restart background service automatically.");
        eprintln!("  Run `autter bg restart` if cloud sync does not resume.");
        return;
    };

    if let Err(error) = crate::commands::daemon::restart_daemon(&daemon_config) {
        eprintln!("  Warning: could not restart background service: {error}");
        eprintln!("  Run `autter bg restart` if cloud sync does not resume.");
    }
}

/// Extract a `--token <value>` or `--token=<value>` argument, if present.
fn parse_token_arg(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(rest) = arg.strip_prefix("--token=") {
            return Some(rest.to_string());
        }
        if arg == "--token" {
            return args.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

/// The Autter web dashboard, where the user creates a Personal Access Token.
const DEFAULT_WEB_APP_URL: &str = "https://app.autter.dev";

/// Resolve the web dashboard URL. Precedence:
///   1. `AUTTER_WEB_URL` env (explicit override, e.g. a local Vite dev server)
///   2. derived from the configured `api_base_url` (swap the `api` host label)
///   3. the default `https://app.autter.dev`
pub(crate) fn web_app_url() -> String {
    if let Ok(url) = std::env::var("AUTTER_WEB_URL")
        && !url.trim().is_empty()
    {
        return url;
    }
    if let Some(web) = derive_web_url_from_api(crate::config::Config::get().api_base_url()) {
        return web;
    }
    DEFAULT_WEB_APP_URL.to_string()
}

/// Derive the web app URL from the API base URL by swapping the leading `api`
/// host label for `app`, e.g. `https://test-api.autter.dev` ->
/// `https://test-app.autter.dev`, `https://api.autter.dev` -> `https://app.autter.dev`.
/// Returns `None` when there is no `api` label to swap.
fn derive_web_url_from_api(api_base_url: &str) -> Option<String> {
    let (scheme, rest) = api_base_url.split_once("://")?;
    let (host, tail) = match rest.split_once('/') {
        Some((h, t)) => (h, Some(t)),
        None => (rest, None),
    };
    let (first, remainder) = host.split_once('.')?;
    let new_first = if first == "api" {
        "app".to_string()
    } else if let Some(prefix) = first.strip_suffix("-api") {
        format!("{prefix}-app")
    } else {
        return None;
    };
    let new_host = format!("{new_first}.{remainder}");
    Some(match tail {
        Some(t) => format!("{scheme}://{new_host}/{t}"),
        None => format!("{scheme}://{new_host}"),
    })
}

/// Print the PAT fallback for when the interactive device flow cannot complete.
fn print_pat_fallback_instructions() {
    eprintln!("You can retry with `autter login`, or sign in with a Personal Access Token:\n");
    eprintln!("  1. Open the Autter dashboard:");
    eprintln!("       {}", web_app_url());
    eprintln!("  2. Open any organization's Settings -> Access Tokens");
    eprintln!("  3. Click \"Create token\", copy it, and run:");
    eprintln!("       autter login --token <paste-your-token>\n");
}

/// Handle the `autter login` command.
///
/// Default path is the OAuth2 device-authorization flow: the CLI opens a
/// browser, the user approves the device, and credentials are stored with no
/// token paste. `--token <PAT>` remains the non-interactive / CI fallback.
pub fn handle_login(args: &[String]) {
    if args
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "help")
    {
        crate::commands::arg_parser::print_command_help("login");
        return;
    }

    if let Some(token) = parse_token_arg(args) {
        if let Err(e) = run_pat_login(&token) {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return;
    }

    match run_device_login() {
        Ok(LoginOutcome::AlreadyLoggedIn) => {
            eprintln!("Already logged in. Use 'autter logout' to log out first.");
        }
        Ok(LoginOutcome::LoggedIn) => {}
        Err(e) => {
            eprintln!("{}", e);
            eprintln!();
            print_pat_fallback_instructions();
            std::process::exit(1);
        }
    }
}

/// Attempt to open a URL in the system's default browser
fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = std::process::Command::new("open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        cmd
    };

    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_arg_supports_space_and_equals() {
        assert_eq!(
            parse_token_arg(&["--token".into(), "autter_pat_abc".into()]),
            Some("autter_pat_abc".into())
        );
        assert_eq!(
            parse_token_arg(&["--token=autter_pat_abc".into()]),
            Some("autter_pat_abc".into())
        );
        assert_eq!(parse_token_arg(&["--json".into()]), None);
    }

    #[test]
    fn derive_web_url_swaps_api_host_label() {
        assert_eq!(
            derive_web_url_from_api("https://api.autter.dev"),
            Some("https://app.autter.dev".into())
        );
        assert_eq!(
            derive_web_url_from_api("https://test-api.autter.dev"),
            Some("https://test-app.autter.dev".into())
        );
        assert_eq!(derive_web_url_from_api("https://example.com"), None);
    }
}
