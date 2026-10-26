use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::auth::types::StoredCredentials;
use crate::auth::{CredentialStore, OAuthClient};
use crate::ui::signin::{self, SigninEnd, SigninScreen};
use crate::ui::{self, SelectItem, rail};

/// Result of running the OAuth device-login flow.
pub enum LoginOutcome {
    /// Valid credentials were already present; no new login was performed.
    AlreadyLoggedIn,
    /// A fresh login completed successfully and credentials were stored.
    LoggedIn,
    /// The user chose not to finish signing in: Esc on the sign-in screen,
    /// or the last choice after a failed attempt (whose error is kept).
    Declined { after_error: Option<String> },
}

/// What stepping away from an interactive sign-in means to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decline {
    /// `autter onboard`: set up in local mode instead.
    ContinueLocally,
    /// `autter login`: stop.
    Cancel,
}

impl Decline {
    fn escape_label(self) -> &'static str {
        match self {
            Decline::ContinueLocally => "continue in local mode",
            Decline::Cancel => "cancel",
        }
    }
}

/// How the caller frames an interactive sign-in.
#[derive(Debug, Clone, Copy)]
pub struct LoginUi<'a> {
    /// Step counter shown beside the title, e.g. `2/4`.
    pub counter: Option<&'a str>,
    pub decline: Decline,
}

/// Run the OAuth2 device-authorization flow as `autter login` does.
///
/// Unlike [`handle_login`], this never calls `std::process::exit` (except on
/// Ctrl-C), so it can be reused by other flows. See [`run_device_login_with`].
pub fn run_device_login() -> Result<LoginOutcome, String> {
    run_device_login_with(LoginUi {
        counter: None,
        decline: Decline::Cancel,
    })
}

/// Run the OAuth2 device-authorization flow.
///
/// At a terminal this shows the sign-in screen (code, countdown, keys to
/// reopen the browser or copy the code) and, when an attempt fails, offers to
/// try again, paste an access token, or step away. Without a terminal it
/// prints the URL and code and waits, returning the error if it fails.
pub fn run_device_login_with(login_ui: LoginUi) -> Result<LoginOutcome, String> {
    let store = CredentialStore::new();

    let stored = store.load().ok().flatten();
    if let Some(refusal) = stored
        .as_ref()
        .and_then(|creds| creds.refresh_failed.as_ref())
    {
        // Refused credentials are never "already logged in", whatever their expiry says.
        eprintln!(
            "The server refused your saved login: {}\nStarting a new sign-in...\n",
            refusal.message()
        );
    } else if let Some(creds) = &stored
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
            resume_cloud_sync_after_login(false);
            return Ok(LoginOutcome::LoggedIn);
        }
        eprintln!("Stored credentials could not authenticate. Starting a new sign-in...\n");
    }

    if ui::is_interactive() {
        return run_interactive(&store, login_ui);
    }

    let client = OAuthClient::new();

    // Start device flow
    eprintln!("Starting device authorization...\n");

    let auth_response = client.start_device_flow().map_err(|e| {
        cli_authorization_failure(format!("could not start the browser sign-in: {e}"))
    })?;

    // Build the display URL
    let display_url = auth_response
        .verification_uri_complete
        .as_ref()
        .unwrap_or(&auth_response.verification_uri);

    // Display instructions
    let caps = crate::ui::caps();
    eprintln!("To authorize this device:");
    eprintln!("  1. Open this URL in your browser:");
    eprintln!("     {}", caps.paint("36", display_url));
    eprintln!();
    eprintln!("  2. Enter this code when prompted:");
    eprintln!("     {}", caps.paint("1", &auth_response.user_code));
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
            creds
        }
        Err(e) => {
            spinner.stop();
            return Err(cli_authorization_failure(e));
        }
    };

    // Authorization is only successful once the credentials are safely stored.
    // Reporting success before this point leaves the user with a browser approval
    // but no durable CLI session.
    store.store(&creds).map_err(|e| {
        format!(
            "CLI authorization failed: browser approval succeeded, but credentials could not be saved: {e}\nExisting stored credentials were not changed."
        )
    })?;

    crate::auth::notice::clear_sync_auth_blocked();
    let ok = caps.glyphs().ok;
    eprintln!(
        "{}",
        caps.paint("1;32", &format!("{ok} CLI authorization successful."))
    );
    print_login_identity(&creds.access_token);
    resume_cloud_sync_after_login(false);

    Ok(LoginOutcome::LoggedIn)
}

/// The interactive sign-in loop: browser sign-in, then on failure a choice
/// to retry, paste a token, or step away.
fn run_interactive(store: &CredentialStore, login_ui: LoginUi) -> Result<LoginOutcome, String> {
    loop {
        let error = match sign_in_with_browser(login_ui) {
            SigninEnd::Approved(creds) => return finish_interactive(store, &creds, login_ui),
            SigninEnd::Escaped => {
                if login_ui.decline == Decline::ContinueLocally {
                    rail::print_step("Sign in \u{00B7} skipped", login_ui.counter);
                }
                return Ok(LoginOutcome::Declined { after_error: None });
            }
            SigninEnd::Failed(error) => error,
        };

        rail::print_warn(&format!("Sign-in didn't finish: {error}"));
        match failure_choice(login_ui.decline) {
            FailureChoice::Retry => {}
            FailureChoice::Token => {
                let Some(token) = ui::secret("Paste your access token") else {
                    continue;
                };
                match exchange_pat(&token) {
                    Ok(creds) => return finish_interactive(store, &creds, login_ui),
                    Err(e) => rail::print_warn(&e),
                }
            }
            FailureChoice::Decline => {
                return Ok(LoginOutcome::Declined {
                    after_error: Some(cli_authorization_failure(&error)),
                });
            }
        }
    }
}

/// Start a device sign-in, open the browser, and wait on the sign-in screen
/// while a background thread polls for approval.
fn sign_in_with_browser(login_ui: LoginUi) -> SigninEnd<StoredCredentials> {
    let spinner = ui::progress::Spinner::new("Starting sign-in\u{2026}");
    let started = OAuthClient::new().start_device_flow();
    spinner.stop();
    let auth = match started {
        Ok(auth) => auth,
        Err(e) => return SigninEnd::Failed(format!("could not start the browser sign-in: {e}")),
    };

    let url = auth
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| auth.verification_uri.clone());
    let opened = open_browser(&url).is_ok();

    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    {
        let cancel = Arc::clone(&cancel);
        let device_code = auth.device_code.clone();
        let (interval, expires_in) = (auth.interval, auth.expires_in);
        std::thread::spawn(move || {
            let result = OAuthClient::new().poll_for_token_until(
                &device_code,
                interval,
                expires_in,
                &cancel,
            );
            let _ = tx.send(result);
        });
    }

    let screen = SigninScreen {
        title: rail::titled(&ui::caps(), "Sign in to Autter", login_ui.counter),
        code: &auth.user_code,
        url: &url,
        browser_opened: opened,
        expires_in: Duration::from_secs(u64::from(auth.expires_in)),
        escape_label: login_ui.decline.escape_label(),
    };
    let end = signin::wait(&screen, rx, || open_browser(&url).is_ok());
    cancel.store(true, Ordering::Relaxed);
    end
}

/// Store approved credentials and show who is signed in.
fn finish_interactive(
    store: &CredentialStore,
    creds: &StoredCredentials,
    login_ui: LoginUi,
) -> Result<LoginOutcome, String> {
    // Authorization is only successful once the credentials are safely stored.
    store.store(creds).map_err(|e| {
        format!(
            "CLI authorization failed: approval succeeded, but credentials could not be saved: {e}\nExisting stored credentials were not changed."
        )
    })?;
    crate::auth::notice::clear_sync_auth_blocked();
    rail::print_step(
        &format!("Signed in as {}", identity_summary(&creds.access_token)),
        login_ui.counter,
    );
    resume_cloud_sync_after_login(true);
    Ok(LoginOutcome::LoggedIn)
}

enum FailureChoice {
    Retry,
    Token,
    Decline,
}

fn failure_choice(decline: Decline) -> FailureChoice {
    let last = match decline {
        Decline::ContinueLocally => SelectItem::new(
            "Continue in local mode",
            "Connect later with `autter onboard --connect`.",
        ),
        Decline::Cancel => SelectItem::new("Cancel", "Run `autter login` again anytime."),
    };
    let token_hint = format!(
        "Create one at {} under any organization's Settings \u{2192} Access Tokens.",
        web_app_url()
    );
    let items = [
        SelectItem::new(
            "Try again with a new code",
            "Opens a fresh sign-in in your browser.",
        ),
        SelectItem::new("Paste an access token", &token_hint),
        last,
    ];
    match ui::select("What next?", &items, 0) {
        0 => FailureChoice::Retry,
        1 => FailureChoice::Token,
        _ => FailureChoice::Decline,
    }
}

fn cli_authorization_failure(detail: impl std::fmt::Display) -> String {
    format!("CLI authorization failed: {detail}\nNo credentials were issued or changed.")
}

/// Exchange a Personal Access Token for credentials, validating it up-front
/// so a bad token fails immediately rather than being stored.
fn exchange_pat(token: &str) -> Result<StoredCredentials, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("No token provided. Usage: autter login --token <token>".to_string());
    }
    OAuthClient::new().exchange_pat(token)
}

/// Sign in with a Personal Access Token instead of the interactive device flow.
pub fn run_pat_login(token: &str) -> Result<LoginOutcome, String> {
    let creds = exchange_pat(token)?;

    let store = CredentialStore::new();
    store
        .store(&creds)
        .map_err(|e| format!("Failed to store credentials: {}", e))?;
    crate::auth::notice::clear_sync_auth_blocked();

    eprintln!("Successfully logged in with a Personal Access Token.");
    print_login_identity(&creds.access_token);
    resume_cloud_sync_after_login(false);
    Ok(LoginOutcome::LoggedIn)
}

/// "Name (email) · Org" from the access token's claims, best-effort.
fn identity_summary(access_token: &str) -> String {
    let identity = crate::auth::identity::extract_identity_from_access_token(access_token);
    let name = identity.name.as_deref().filter(|s| !s.is_empty());
    let email = identity.email.as_deref().filter(|s| !s.is_empty());
    let mut summary = match (name, email) {
        (Some(name), Some(email)) => format!("{name} ({email})"),
        (Some(who), None) | (None, Some(who)) => who.to_string(),
        (None, None) => "your Autter account".to_string(),
    };
    if let Some(org) = identity.active_org()
        && let Some(org_name) = org.org_name.as_deref().filter(|s| !s.is_empty())
    {
        summary.push_str(&format!(" \u{00B7} {org_name}"));
    }
    summary
}

/// Print the signed-in user and active org from the access token's claims
/// (best-effort — falls back gracefully).
fn print_login_identity(access_token: &str) {
    use crate::auth::identity::extract_identity_from_access_token;

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
/// `on_rail` prints the notes under an interactive flow's rail.
fn resume_cloud_sync_after_login(on_rail: bool) {
    if std::env::var_os("AUTTER_TEST_DB_PATH").is_some() {
        return;
    }
    let say = |text: &str| {
        if on_rail {
            rail::print_note(text);
        } else {
            eprintln!("  {text}");
        }
    };

    let pending = crate::auth::notice::pending_sync_counts();
    if pending.total() > 0 {
        say(&format!(
            "{} queued locally \u{2014} restarting background service to upload.",
            pending.summary()
        ));
    } else {
        say("Restarting background service to resume cloud sync.");
    }

    let Ok(daemon_config) = crate::daemon::DaemonConfig::from_env_or_default_paths() else {
        say("Warning: could not restart background service automatically.");
        say("Run `autter bg restart` if cloud sync does not resume.");
        return;
    };

    if let Err(error) = crate::commands::daemon::restart_daemon(&daemon_config) {
        say(&format!(
            "Warning: could not restart background service: {error}"
        ));
        say("Run `autter bg restart` if cloud sync does not resume.");
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
pub fn print_pat_fallback_instructions() {
    eprintln!("Interactive login didn't finish. Paste an access token on this same step:\n");
    eprintln!("  1. Open the Autter dashboard:");
    eprintln!("       {}", web_app_url());
    eprintln!("  2. Open any organization's Settings -> Access Tokens");
    eprintln!("  3. Click \"Create token\", copy it, and run:");
    eprintln!("       autter login --token <paste-your-token>\n");
    eprintln!("  (This is a CLI/MCP access token — not a Runtime ingest key.)\n");
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
        Ok(LoginOutcome::LoggedIn) => {
            if ui::is_interactive() {
                rail::print_end("You're signed in.");
            }
        }
        Ok(LoginOutcome::Declined { after_error }) => {
            rail::print_end("Sign-in cancelled.");
            std::process::exit(if after_error.is_some() { 1 } else { 130 });
        }
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

    #[test]
    fn authorization_failure_is_explicit_about_credential_state() {
        let message = cli_authorization_failure("the request was denied");

        assert_eq!(
            message,
            "CLI authorization failed: the request was denied\nNo credentials were issued or changed."
        );
    }
}
