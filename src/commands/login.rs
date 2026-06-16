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

/// Handle the `autter login` command
pub fn handle_login(_args: &[String]) {
    match run_device_login() {
        Ok(LoginOutcome::AlreadyLoggedIn) => {
            eprintln!("Already logged in. Use 'autter logout' to log out first.");
            std::process::exit(0);
        }
        Ok(LoginOutcome::LoggedIn) => {
            eprintln!("\nSuccessfully logged in!");
        }
        Err(e) => {
            eprintln!("\n{}", e);
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
