/// Handle the `autter personal-dashboard` command
pub fn handle_personal_dashboard(_args: &[String]) {
    let dashboard_url = personal_dashboard_url();

    eprintln!("Opening dashboard: {}", dashboard_url);

    if open_browser(&dashboard_url).is_err() {
        eprintln!("Could not open browser automatically.");
        eprintln!("Visit this URL in your browser:");
        eprintln!("  {}", dashboard_url);
    }
}

/// Resolve the user-facing personal dashboard URL.
///
/// This must use the web-app base ([`crate::commands::login::web_app_url`]),
/// not the API base: `api.autter.dev/me` 404s while `app.autter.dev/me` is
/// the working page.
pub(crate) fn personal_dashboard_url() -> String {
    build_personal_dashboard_url(&crate::commands::login::web_app_url())
}

fn build_personal_dashboard_url(web_app_base: &str) -> String {
    format!("{}/me", web_app_base.trim_end_matches('/'))
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

    #[test]
    fn builds_personal_dashboard_url_from_web_app_base() {
        assert_eq!(
            build_personal_dashboard_url("https://app.autter.dev"),
            "https://app.autter.dev/me"
        );
        // Trailing slashes must not produce a double slash.
        assert_eq!(
            build_personal_dashboard_url("https://app.autter.dev/"),
            "https://app.autter.dev/me"
        );
        // Staging-style hosts keep their prefix.
        assert_eq!(
            build_personal_dashboard_url("https://test-app.autter.dev"),
            "https://test-app.autter.dev/me"
        );
    }

    #[test]
    fn personal_dashboard_url_uses_app_host_not_api_host() {
        let url = personal_dashboard_url();
        assert!(
            url.ends_with("/me"),
            "personal dashboard URL should end with /me, got {url}"
        );
        // The opened URL must be derived from the web-app base, not the API base.
        let expected = build_personal_dashboard_url(&crate::commands::login::web_app_url());
        assert_eq!(url, expected);
        // With the default API base (https://api.autter.dev) the API-derived
        // URL would be https://api.autter.dev/me (404); the web-app swap must
        // have removed the `api` host label whenever one was present.
        let api_base = crate::config::Config::fresh()
            .api_base_url()
            .trim_end_matches('/')
            .to_string();
        if api_base.contains("api.") || api_base.contains("-api.") {
            assert!(
                !url.contains("api."),
                "personal dashboard URL must not use the API host, got {url}"
            );
        }
    }
}
