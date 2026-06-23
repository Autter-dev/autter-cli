use crate::api::client::ApiContext;
use crate::auth::types::{DeviceAuthResponse, OAuthError, StoredCredentials, TokenResponse};
use crate::config;
use crate::http;
use std::thread;
use std::time::Duration;

/// OAuth client for device authorization flow
pub struct OAuthClient {
    base_url: String,
}

/// Validate that a URL uses HTTPS (security requirement for OAuth)
/// In release builds, only HTTPS is accepted — the HTTP path is not compiled in.
/// In debug builds, HTTP is also allowed for local development.
#[cfg(not(debug_assertions))]
fn validate_https_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err(format!(
            "Security error: OAuth requires HTTPS. URL '{}' is not secure.",
            url
        ));
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn validate_https_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(format!("Invalid URL scheme: {}", url));
    }
    Ok(())
}

impl OAuthClient {
    /// Create a new OAuth client using the current config
    /// Uses Config::fresh() to support runtime config updates (daemon mode)
    pub fn new() -> Self {
        let config = config::Config::fresh();
        let base_url = config.api_base_url().to_string();

        // Validate HTTPS in release mode (panics on invalid URL - fail-safe)
        if let Err(e) = validate_https_url(&base_url) {
            panic!("{}", e);
        }

        Self { base_url }
    }

    /// Create an OAuthClient with a custom base URL (for install script flow)
    pub fn with_base_url(base_url: &str) -> Result<Self, String> {
        validate_https_url(base_url)?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Common token exchange logic - POST to /worker/oauth/token with given body
    fn exchange_token(&self, body: serde_json::Value) -> Result<StoredCredentials, String> {
        let url = format!("{}/worker/oauth/token", self.base_url);

        let (_agent, request) = ApiContext::http_post(&url, Some(30));
        let request = request.set("Content-Type", "application/json");
        let response = http::send_with_body(request, &body.to_string())
            .map_err(|e| format!("Failed to connect to server: {}", e))?;

        let response_body = response
            .as_str()
            .map_err(|e| format!("Invalid response encoding: {}", e))?;

        if response.status_code != 200 {
            let error: OAuthError = serde_json::from_str(response_body).unwrap_or(OAuthError {
                error: "unknown_error".to_string(),
                error_description: None,
            });

            let msg = error
                .error_description
                .unwrap_or_else(|| error.error.clone());
            return Err(msg);
        }

        let token_response: TokenResponse = serde_json::from_str(response_body)
            .map_err(|e| format!("Invalid token response: {}", e))?;

        let now = chrono::Utc::now().timestamp();
        Ok(StoredCredentials {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token,
            access_token_expires_at: now + token_response.expires_in as i64,
            refresh_token_expires_at: now + token_response.refresh_expires_in as i64,
        })
    }

    /// Start the device authorization flow
    /// Returns (device_code, user_code, verification_url, expires_in, interval)
    pub fn start_device_flow(&self) -> Result<DeviceAuthResponse, String> {
        let url = format!("{}/worker/oauth/device/code", self.base_url);

        // Self-report device metadata so the approval screen and the persisted
        // device record can show a meaningful "which device" (best-effort).
        let device_name =
            crate::api::client::resolve_hostname().unwrap_or_else(|| "autter CLI".to_string());
        let body = serde_json::json!({
            "client_id": "autter-cli",
            "device_name": device_name,
            "os": std::env::consts::OS,
            "cli_version": env!("CARGO_PKG_VERSION"),
        });

        let (_agent, request) = ApiContext::http_post(&url, Some(30));
        let request = request.set("Content-Type", "application/json");
        let response = http::send_with_body(request, &body.to_string())
            .map_err(|e| format!("Failed to connect to server: {}", e))?;

        if response.status_code != 200 {
            return Err(format!(
                "Server error ({}): {}",
                response.status_code,
                response.as_str().unwrap_or("Unknown error")
            ));
        }

        let body = response
            .as_str()
            .map_err(|e| format!("Invalid response encoding: {}", e))?;

        serde_json::from_str::<DeviceAuthResponse>(body)
            .map_err(|e| format!("Invalid response from server: {}", e))
    }

    /// Poll for token with the given device code
    /// Implements RFC 8628 polling with proper error handling
    pub fn poll_for_token(
        &self,
        device_code: &str,
        interval: u32,
        expires_in: u32,
    ) -> Result<StoredCredentials, String> {
        let url = format!("{}/worker/oauth/token", self.base_url);
        let mut elapsed = 0u32;
        let mut current_interval = interval;

        while elapsed < expires_in {
            // Wait before polling
            thread::sleep(Duration::from_secs(current_interval as u64));
            elapsed += current_interval;

            let body = serde_json::json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                "device_code": device_code,
                "client_id": "autter-cli"
            });

            let (_agent, request) = ApiContext::http_post(&url, Some(30));
            let request = request.set("Content-Type", "application/json");
            let response = http::send_with_body(request, &body.to_string())
                .map_err(|e| format!("Failed to connect to server: {}", e))?;

            let response_body = response
                .as_str()
                .map_err(|e| format!("Invalid response encoding: {}", e))?;

            if response.status_code == 200 {
                let token_response: TokenResponse = serde_json::from_str(response_body)
                    .map_err(|e| format!("Invalid token response: {}", e))?;

                let now = chrono::Utc::now().timestamp();
                return Ok(StoredCredentials {
                    access_token: token_response.access_token,
                    refresh_token: token_response.refresh_token,
                    access_token_expires_at: now + token_response.expires_in as i64,
                    refresh_token_expires_at: now + token_response.refresh_expires_in as i64,
                });
            }

            // Parse error response
            let error: OAuthError = match serde_json::from_str(response_body) {
                Ok(e) => e,
                Err(_) => {
                    return Err(format!("Server error ({})", response.status_code));
                }
            };

            match error.error.as_str() {
                "authorization_pending" => {
                    // Keep polling - user hasn't approved yet
                    continue;
                }
                "slow_down" => {
                    // Increase interval by 5 seconds per RFC 8628
                    current_interval += 5;
                    continue;
                }
                "access_denied" => {
                    return Err("Authorization was denied".to_string());
                }
                "expired_token" => {
                    return Err("Device code expired. Please try again.".to_string());
                }
                _ => {
                    let msg = error
                        .error_description
                        .unwrap_or_else(|| error.error.clone());
                    return Err(format!("Authorization failed: {}", msg));
                }
            }
        }

        Err("Device code expired. Please try again.".to_string())
    }

    /// Refresh the access token using a refresh token
    pub fn refresh_access_token(&self, refresh_token: &str) -> Result<StoredCredentials, String> {
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": "autter-cli"
        });

        self.exchange_token(body)
            .map_err(|e| format!("Token refresh failed: {}", e))
    }

    /// Exchange a Personal Access Token (PAT) for credentials.
    ///
    /// The PAT rides the refresh grant — the backend routes it by prefix and
    /// returns it unchanged (PATs are stable, not rotated). Used by
    /// `autter login --token <PAT>` to validate the token and seed credentials.
    pub fn exchange_pat(&self, pat: &str) -> Result<StoredCredentials, String> {
        self.exchange_pat_for_org(pat, None)
    }

    /// Exchange a PAT for credentials scoped to a specific org.
    ///
    /// `org_id = None` mints for the token's home org. A specific `org_id` mints
    /// for that org (the backend verifies the PAT owner is a member) — this is how
    /// the CLI routes a push to the org that owns the current repository.
    pub fn exchange_pat_for_org(
        &self,
        pat: &str,
        org_id: Option<&str>,
    ) -> Result<StoredCredentials, String> {
        let mut body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": pat,
            "client_id": "autter-cli"
        });
        if let Some(org) = org_id {
            body["org_id"] = serde_json::Value::String(org.to_string());
        }

        self.exchange_token(body)
            .map_err(|e| format!("Token sign-in failed: {}", e))
    }

    /// Resolve which org owns a repository (by `owner/repo` or remote URL), so a
    /// push can be routed to the right org. Returns `None` when no org tracks it
    /// (the caller should fall back to the PAT's home org).
    pub fn resolve_org_for_repo(&self, pat: &str, repo: &str) -> Result<Option<String>, String> {
        let url = format!("{}/worker/oauth/resolve-org", self.base_url);
        let body = serde_json::json!({ "pat": pat, "repo": repo });

        let (_agent, request) = ApiContext::http_post(&url, Some(30));
        let request = request.set("Content-Type", "application/json");
        let response = http::send_with_body(request, &body.to_string())
            .map_err(|e| format!("Failed to connect to server: {}", e))?;

        if response.status_code != 200 {
            return Err(format!("resolve-org failed ({})", response.status_code));
        }

        let body = response
            .as_str()
            .map_err(|e| format!("Invalid response encoding: {}", e))?;

        #[derive(serde::Deserialize)]
        struct ResolveOrgResponse {
            org_id: Option<String>,
        }
        let parsed: ResolveOrgResponse = serde_json::from_str(body)
            .map_err(|e| format!("Invalid resolve-org response: {}", e))?;
        Ok(parsed.org_id)
    }

    /// Exchange an install nonce for credentials (auto-login from web install page)
    pub fn exchange_install_nonce(&self, nonce: &str) -> Result<StoredCredentials, String> {
        let body = serde_json::json!({
            "grant_type": "install_nonce",
            "install_nonce": nonce,
            "client_id": "autter-cli"
        });

        self.exchange_token(body)
            .map_err(|e| format!("Nonce exchange failed: {}", e))
    }
}

impl Default for OAuthClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============= URL Validation Tests =============

    #[test]
    fn test_validate_https_url_valid() {
        let result = validate_https_url("https://example.com");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_https_url_with_path() {
        let result = validate_https_url("https://example.com/api/v1");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_https_url_with_port() {
        let result = validate_https_url("https://example.com:8443/api");
        assert!(result.is_ok());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn test_validate_https_url_http_allowed_in_debug() {
        // In debug mode, HTTP should be allowed for local development
        let result = validate_https_url("http://localhost:8080");
        assert!(result.is_ok());
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn test_validate_https_url_http_rejected_in_release() {
        // In release mode, HTTP should be rejected
        let result = validate_https_url("http://example.com");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("HTTPS"));
    }

    #[test]
    fn test_validate_https_url_invalid_scheme() {
        let result = validate_https_url("ftp://example.com");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_https_url_no_scheme() {
        let result = validate_https_url("example.com");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_https_url_empty() {
        let result = validate_https_url("");
        assert!(result.is_err());
    }

    // ============= Token Response Parsing Tests =============

    #[test]
    fn test_parse_token_response_valid() {
        let json = r#"{
            "access_token": "test_access",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "test_refresh",
            "refresh_expires_in": 7776000
        }"#;

        let result: Result<TokenResponse, _> = serde_json::from_str(json);
        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.access_token, "test_access");
        assert_eq!(response.refresh_token, "test_refresh");
        assert_eq!(response.expires_in, 3600);
        assert_eq!(response.refresh_expires_in, 7776000);
    }

    #[test]
    fn test_parse_token_response_missing_field() {
        // Missing refresh_expires_in
        let json = r#"{
            "access_token": "test_access",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "test_refresh"
        }"#;

        let result: Result<TokenResponse, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_token_response_wrong_types() {
        // expires_in as string instead of number
        let json = r#"{
            "access_token": "test_access",
            "token_type": "Bearer",
            "expires_in": "3600",
            "refresh_token": "test_refresh",
            "refresh_expires_in": 7776000
        }"#;

        let result: Result<TokenResponse, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // ============= OAuth Error Parsing Tests =============

    #[test]
    fn test_parse_oauth_error_with_description() {
        let json = r#"{
            "error": "invalid_grant",
            "error_description": "The refresh token is expired"
        }"#;

        let result: Result<OAuthError, _> = serde_json::from_str(json);
        assert!(result.is_ok());

        let error = result.unwrap();
        assert_eq!(error.error, "invalid_grant");
        assert_eq!(
            error.error_description,
            Some("The refresh token is expired".to_string())
        );
    }

    #[test]
    fn test_parse_oauth_error_without_description() {
        let json = r#"{"error": "access_denied"}"#;

        let result: Result<OAuthError, _> = serde_json::from_str(json);
        assert!(result.is_ok());

        let error = result.unwrap();
        assert_eq!(error.error, "access_denied");
        assert!(error.error_description.is_none());
    }

    // ============= Device Auth Response Parsing Tests =============

    #[test]
    fn test_parse_device_auth_response_valid() {
        let json = r#"{
            "device_code": "abc123",
            "user_code": "WXYZ-1234",
            "verification_uri": "https://example.com/verify",
            "verification_uri_complete": "https://example.com/verify?code=WXYZ-1234",
            "expires_in": 900,
            "interval": 5
        }"#;

        let result: Result<DeviceAuthResponse, _> = serde_json::from_str(json);
        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.device_code, "abc123");
        assert_eq!(response.user_code, "WXYZ-1234");
        assert_eq!(response.interval, 5);
        assert_eq!(response.expires_in, 900);
    }

    #[test]
    fn test_parse_device_auth_response_without_optional_uri() {
        let json = r#"{
            "device_code": "abc123",
            "user_code": "WXYZ-1234",
            "verification_uri": "https://example.com/verify",
            "expires_in": 900,
            "interval": 5
        }"#;

        let result: Result<DeviceAuthResponse, _> = serde_json::from_str(json);
        assert!(result.is_ok());

        let response = result.unwrap();
        assert!(response.verification_uri_complete.is_none());
    }

    // ============= Credentials Calculation Tests =============

    #[test]
    fn test_credentials_expiry_calculation() {
        // Test that credentials are calculated correctly from token response
        let now = chrono::Utc::now().timestamp();
        let expires_in: u64 = 3600;
        let refresh_expires_in: u64 = 7776000;

        let creds = StoredCredentials {
            access_token: "test".to_string(),
            refresh_token: "test".to_string(),
            access_token_expires_at: now + expires_in as i64,
            refresh_token_expires_at: now + refresh_expires_in as i64,
        };

        // Access token should expire in about 1 hour
        assert!(creds.access_token_expires_at > now);
        assert!(creds.access_token_expires_at <= now + 3601);

        // Refresh token should expire in about 90 days
        assert!(creds.refresh_token_expires_at > now + 86400 * 89);
        assert!(creds.refresh_token_expires_at <= now + 86400 * 91);
    }
}
