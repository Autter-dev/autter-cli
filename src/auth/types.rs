use serde::{Deserialize, Serialize};
use std::fmt;

/// Stored credentials for OAuth tokens
/// NOTE: Debug intentionally redacts tokens to prevent accidental exposure in logs
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredCredentials {
    /// The access token (short-lived, 1 hour)
    pub access_token: String,
    /// The refresh token (long-lived, 90 days)
    pub refresh_token: String,
    /// Unix timestamp when the access token expires
    pub access_token_expires_at: i64,
    /// Unix timestamp when the refresh token expires
    pub refresh_token_expires_at: i64,
    /// Set once the server refuses `refresh_token` for good (`invalid_grant`).
    /// Every process then stops refreshing until `autter login` stores new
    /// credentials, which never carry this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_failed: Option<RefreshFailure>,
}

/// Custom Debug implementation that redacts sensitive token values
impl fmt::Debug for StoredCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredCredentials")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("access_token_expires_at", &self.access_token_expires_at)
            .field("refresh_token_expires_at", &self.refresh_token_expires_at)
            .field("refresh_failed", &self.refresh_failed)
            .finish()
    }
}

/// The server's terminal refusal of a stored refresh token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshFailure {
    /// Unix timestamp of the refusal.
    pub at: i64,
    /// OAuth `error`, normally `invalid_grant`.
    pub error: String,
    /// Server `error_code` (e.g. `refresh_token_invalid`); older servers omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Server `error_description`, shown to the user as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl RefreshFailure {
    pub fn from_oauth_error(error: &OAuthError) -> Self {
        Self {
            at: chrono::Utc::now().timestamp(),
            error: error.error.clone(),
            error_code: error.error_code.clone(),
            description: error.error_description.clone(),
        }
    }

    /// What to tell the user: the server's own wording when it sent one.
    pub fn message(&self) -> String {
        match self.description.as_deref().map(str::trim) {
            Some(description) if !description.is_empty() => description.to_string(),
            _ => format!(
                "The server no longer accepts this login ({}). Run `autter login` to sign in again.",
                self.error
            ),
        }
    }
}

impl StoredCredentials {
    /// True once the server refused the refresh token; only `autter login` fixes it.
    pub fn is_login_required(&self) -> bool {
        self.refresh_failed.is_some()
    }

    /// Check if the access token is expired or will expire within the given buffer (seconds)
    pub fn is_access_token_expired(&self, buffer_secs: i64) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.access_token_expires_at <= now + buffer_secs
    }

    /// Check if the refresh token is expired
    pub fn is_refresh_token_expired(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.refresh_token_expires_at <= now
    }
}

/// Response from device authorization endpoint
#[derive(Debug, Deserialize)]
pub struct DeviceAuthResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u32,
    pub interval: u32,
}

/// Response from token endpoint
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[allow(dead_code)]
    pub token_type: String,
    pub expires_in: u64,
    pub refresh_token: String,
    pub refresh_expires_in: u64,
}

/// OAuth error response (RFC 6749 §5.2, plus the autter server's extensions)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OAuthError {
    pub error: String,
    #[serde(default)]
    pub error_description: Option<String>,
    /// Why a refresh token was refused: `refresh_token_invalid`, `device_revoked`,
    /// `refresh_token_expired` or `refresh_token_superseded`.
    #[serde(default)]
    pub error_code: Option<String>,
    /// The server's verdict that only a new `autter login` can recover.
    #[serde(default)]
    pub login_required: Option<bool>,
}

impl OAuthError {
    /// Whether this refusal is terminal for the token that was sent. Servers
    /// before `login_required` existed only send `invalid_grant`.
    pub fn requires_login(&self) -> bool {
        self.login_required.unwrap_or(self.error == "invalid_grant")
    }
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.error_description.as_deref() {
            Some(description) if !description.trim().is_empty() => f.write_str(description),
            _ => f.write_str(&self.error),
        }
    }
}

/// Why a request to the token endpoint failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenRequestError {
    /// No OAuth answer: network failure, unreadable body, or a non-OAuth
    /// error page (e.g. a proxy 502). Retry later.
    Transport(String),
    /// The server answered with an OAuth error body.
    Refused { status: u16, error: OAuthError },
}

impl TokenRequestError {
    /// The parsed OAuth error, when the server sent one.
    pub fn oauth_error(&self) -> Option<&OAuthError> {
        match self {
            Self::Transport(_) => None,
            Self::Refused { error, .. } => Some(error),
        }
    }

    /// True only for a client-error refusal that the server says needs a new
    /// login. Network errors, 5xx, `temporarily_unavailable` and
    /// `access_denied` stay retryable.
    pub fn requires_login(&self) -> bool {
        match self {
            Self::Transport(_) => false,
            Self::Refused { status, error } => {
                (400..500).contains(status) && error.requires_login()
            }
        }
    }
}

impl fmt::Display for TokenRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => f.write_str(message),
            Self::Refused { error, .. } => write!(f, "{error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_credentials(access_expires_at: i64, refresh_expires_at: i64) -> StoredCredentials {
        StoredCredentials {
            access_token: "test_access_token".to_string(),
            refresh_token: "test_refresh_token".to_string(),
            access_token_expires_at: access_expires_at,
            refresh_token_expires_at: refresh_expires_at,
            refresh_failed: None,
        }
    }

    // ============= is_access_token_expired() tests =============

    #[test]
    fn test_access_token_not_expired() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 3600, now + 86400 * 90); // expires in 1 hour
        assert!(!creds.is_access_token_expired(300)); // 5 min buffer
    }

    #[test]
    fn test_access_token_expired() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now - 3600, now + 86400 * 90); // expired 1 hour ago
        assert!(creds.is_access_token_expired(0));
    }

    #[test]
    fn test_access_token_expires_within_buffer() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 200, now + 86400 * 90); // expires in 200s
        assert!(creds.is_access_token_expired(300)); // 300s buffer - should be "expired"
    }

    #[test]
    fn test_access_token_exactly_at_boundary() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 300, now + 86400 * 90); // expires in exactly 300s
        // now + buffer = now + 300, expires_at = now + 300
        // expires_at <= now + buffer, so should be expired (boundary case)
        assert!(creds.is_access_token_expired(300));
    }

    #[test]
    fn test_access_token_one_second_after_boundary() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 301, now + 86400 * 90); // expires in 301s
        assert!(!creds.is_access_token_expired(300)); // 300s buffer - should NOT be expired
    }

    #[test]
    fn test_access_token_zero_buffer() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 1, now + 86400 * 90); // expires in 1s
        assert!(!creds.is_access_token_expired(0)); // no buffer
    }

    #[test]
    fn test_access_token_negative_buffer() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 100, now + 86400 * 90);
        // Negative buffer effectively means we're more lenient
        assert!(!creds.is_access_token_expired(-50));
    }

    #[test]
    fn test_access_token_large_buffer() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 3600, now + 86400 * 90); // expires in 1 hour
        assert!(creds.is_access_token_expired(7200)); // 2 hour buffer - should be "expired"
    }

    // ============= is_refresh_token_expired() tests =============

    #[test]
    fn test_refresh_token_not_expired() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 3600, now + 86400 * 90); // refresh expires in 90 days
        assert!(!creds.is_refresh_token_expired());
    }

    #[test]
    fn test_refresh_token_expired() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 3600, now - 86400); // refresh expired 1 day ago
        assert!(creds.is_refresh_token_expired());
    }

    #[test]
    fn test_refresh_token_exactly_at_boundary() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 3600, now); // refresh expires exactly now
        // expires_at <= now should be true (boundary case)
        assert!(creds.is_refresh_token_expired());
    }

    #[test]
    fn test_refresh_token_one_second_before_expiry() {
        let now = chrono::Utc::now().timestamp();
        let creds = make_credentials(now + 3600, now + 1); // refresh expires in 1s
        assert!(!creds.is_refresh_token_expired());
    }

    // ============= Debug implementation tests =============

    #[test]
    fn test_debug_redacts_access_token() {
        let creds = make_credentials(1000, 2000);
        let debug_output = format!("{:?}", creds);
        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("test_access_token"));
    }

    #[test]
    fn test_debug_redacts_refresh_token() {
        let creds = make_credentials(1000, 2000);
        let debug_output = format!("{:?}", creds);
        assert!(!debug_output.contains("test_refresh_token"));
    }

    #[test]
    fn test_debug_shows_timestamps() {
        let creds = make_credentials(1234567890, 9876543210);
        let debug_output = format!("{:?}", creds);
        assert!(debug_output.contains("1234567890"));
        assert!(debug_output.contains("9876543210"));
    }

    // ============= OAuth error parsing (CE-20) =============

    fn parse_oauth_error(json: &str) -> OAuthError {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn test_current_server_refusal_parses_every_field() {
        let error = parse_oauth_error(
            r#"{"error":"invalid_grant","error_description":"Refresh token is not valid. Run `autter login` to sign in again.","error_code":"refresh_token_invalid","login_required":true}"#,
        );
        assert_eq!(error.error, "invalid_grant");
        assert_eq!(error.error_code.as_deref(), Some("refresh_token_invalid"));
        assert_eq!(error.login_required, Some(true));
        assert!(error.requires_login());
        assert_eq!(
            error.to_string(),
            "Refresh token is not valid. Run `autter login` to sign in again."
        );
    }

    #[test]
    fn test_every_refusal_code_requires_login() {
        for code in [
            "refresh_token_invalid",
            "device_revoked",
            "refresh_token_expired",
            "refresh_token_superseded",
        ] {
            let error = parse_oauth_error(&format!(
                r#"{{"error":"invalid_grant","error_code":"{code}","login_required":true}}"#
            ));
            assert!(error.requires_login(), "{code} should require login");
        }
    }

    #[test]
    fn test_older_server_invalid_grant_alone_requires_login() {
        let error = parse_oauth_error(r#"{"error":"invalid_grant"}"#);
        assert!(error.error_code.is_none());
        assert!(error.login_required.is_none());
        assert!(error.requires_login());
        assert_eq!(error.to_string(), "invalid_grant");

        let error = parse_oauth_error(r#"{"error":"invalid_grant","login_required":null}"#);
        assert!(error.requires_login());
    }

    #[test]
    fn test_login_required_flag_decides_when_present() {
        assert!(
            parse_oauth_error(r#"{"error":"unauthorized_client","login_required":true}"#)
                .requires_login()
        );
        assert!(
            !parse_oauth_error(r#"{"error":"invalid_grant","login_required":false}"#)
                .requires_login()
        );
    }

    #[test]
    fn test_transient_oauth_errors_do_not_require_login() {
        for error in ["temporarily_unavailable", "access_denied", "server_error"] {
            let error = parse_oauth_error(&format!(r#"{{"error":"{error}"}}"#));
            assert!(
                !error.requires_login(),
                "{} should be retryable",
                error.error
            );
        }
    }

    fn refused(status: u16, json: &str) -> TokenRequestError {
        TokenRequestError::Refused {
            status,
            error: parse_oauth_error(json),
        }
    }

    #[test]
    fn test_token_request_error_is_terminal_only_for_client_refusals() {
        assert!(refused(400, r#"{"error":"invalid_grant"}"#).requires_login());
        assert!(refused(401, r#"{"error":"invalid_grant"}"#).requires_login());
        // A 5xx is an outage, whatever its body says.
        assert!(!refused(503, r#"{"error":"invalid_grant"}"#).requires_login());
        assert!(!refused(400, r#"{"error":"temporarily_unavailable"}"#).requires_login());
        assert!(!refused(400, r#"{"error":"access_denied"}"#).requires_login());
        assert!(
            !TokenRequestError::Transport("Failed to connect to server: timed out".to_string())
                .requires_login()
        );
    }

    #[test]
    fn test_token_request_error_exposes_parsed_oauth_error() {
        let error = refused(
            400,
            r#"{"error":"invalid_grant","error_code":"device_revoked","login_required":true}"#,
        );
        let oauth = error.oauth_error().unwrap();
        assert_eq!(oauth.error_code.as_deref(), Some("device_revoked"));
        assert!(
            TokenRequestError::Transport("x".to_string())
                .oauth_error()
                .is_none()
        );
    }

    // ============= Persisted refusal =============

    #[test]
    fn test_refresh_failure_message_prefers_server_description() {
        let failure = RefreshFailure::from_oauth_error(&parse_oauth_error(
            r#"{"error":"invalid_grant","error_description":"Device was revoked.","error_code":"device_revoked"}"#,
        ));
        assert_eq!(failure.message(), "Device was revoked.");
        assert_eq!(failure.error_code.as_deref(), Some("device_revoked"));

        let bare =
            RefreshFailure::from_oauth_error(&parse_oauth_error(r#"{"error":"invalid_grant"}"#));
        assert!(bare.message().contains("invalid_grant"));
        assert!(bare.message().contains("autter login"));
    }

    #[test]
    fn test_credentials_without_refusal_field_still_parse() {
        // Credentials written by earlier CLI versions.
        let creds: StoredCredentials = serde_json::from_str(
            r#"{"access_token":"a","refresh_token":"r","access_token_expires_at":1,"refresh_token_expires_at":2}"#,
        )
        .unwrap();
        assert!(creds.refresh_failed.is_none());
        assert!(!creds.is_login_required());
    }

    #[test]
    fn test_live_credentials_serialize_without_refusal_field() {
        let json = serde_json::to_string(&make_credentials(1, 2)).unwrap();
        assert!(!json.contains("refresh_failed"));
    }

    #[test]
    fn test_refused_credentials_roundtrip() {
        let mut creds = make_credentials(1, 2);
        creds.refresh_failed = Some(RefreshFailure {
            at: 42,
            error: "invalid_grant".to_string(),
            error_code: Some("refresh_token_superseded".to_string()),
            description: Some("Refresh token was replaced.".to_string()),
        });
        let loaded: StoredCredentials =
            serde_json::from_str(&serde_json::to_string(&creds).unwrap()).unwrap();
        assert!(loaded.is_login_required());
        assert_eq!(loaded.refresh_failed, creds.refresh_failed);
    }
}
