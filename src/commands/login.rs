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

    // Check if already logged in
    if let Ok(Some(creds)) = store.load()
        && !creds.is_refresh_token_expired()
    {
        return Ok(LoginOutcome::AlreadyLoggedIn);
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
    eprintln!("     {}", display_url);
    eprintln!();
    eprintln!("  2. Enter this code when prompted:");
    eprintln!("     {}", auth_response.user_code);
    eprintln!();

    // Try to open browser automatically
    if open_browser(display_url).is_err() {
        eprintln!("  (Could not open browser automatically)");
        eprintln!();
    }

    eprintln!("Waiting for authorization...");

    // Poll for token
    let creds = client
        .poll_for_token(
            &auth_response.device_code,
            auth_response.interval,
            auth_response.expires_in,
        )
        .map_err(|e| format!("Authorization failed: {}", e))?;

    // Store credentials (non-fatal on failure)
    if let Err(e) = store.store(&creds) {
        eprintln!("\nWarning: Failed to store credentials: {}", e);
        eprintln!("You may need to log in again next time.");
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

    print_login_success(&creds.access_token);
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
fn web_app_url() -> String {
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

/// Print the step-by-step browser sign-in instructions.
fn print_login_instructions(url: &str) {
    eprintln!("To sign in to Autter:\n");
    eprintln!("  1. We've opened the Autter dashboard in your browser:");
    eprintln!("       {}", url);
    eprintln!("     (If it didn't open, copy that link into your browser.)\n");
    eprintln!("  2. Log in, then open any organization's");
    eprintln!("       Settings -> Access Tokens\n");
    eprintln!("  3. Click \"Create token\", give it a name, and copy the token.\n");
    eprintln!("  4. Come back here and run:");
    eprintln!("       autter login --token <paste-your-token>\n");
}

/// Handle the `autter login` command.
///
/// Two-step, browser-assisted Personal Access Token flow:
///   1. `autter login` opens the dashboard so the user can create + copy a token.
///   2. `autter login --token <PAT>` completes sign-in with that token.
pub fn handle_login(args: &[String]) {
    // Step 2: complete sign-in with a token created in the browser.
    if let Some(token) = parse_token_arg(args) {
        // run_pat_login prints the success message + identity on success.
        if let Err(e) = run_pat_login(&token) {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return;
    }

    // Already signed in? Nothing to do.
    let store = CredentialStore::new();
    if let Ok(Some(creds)) = store.load()
        && !creds.is_refresh_token_expired()
    {
        eprintln!("Already logged in. Use 'autter logout' to log out first.");
        return;
    }

    // Step 1: open the dashboard and tell the user what to do next.
    let url = web_app_url();
    print_login_instructions(&url);
    if open_browser(&url).is_err() {
        eprintln!("  (Could not open the browser automatically — open the link above.)");
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
