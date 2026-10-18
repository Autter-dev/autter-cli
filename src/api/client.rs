use crate::auth::types::{RefreshFailure, StoredCredentials, TokenRequestError};
use crate::auth::{CredentialStore, OAuthClient};
use crate::config;
use crate::error::AutterError;
use crate::git::repository::{current_git_committer_identity_resolution, parse_git_var_identity};
use crate::http;
use once_cell::sync::Lazy;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use url::Url;

/// Global mutex to prevent multiple threads from refreshing simultaneously.
/// This provides in-process synchronization to avoid thundering herd issues.
/// Cross-process refresh is serialized separately via `acquire_refresh_file_lock`
/// because device-flow refresh tokens rotate on every use.
static REFRESH_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// How long to wait for a sibling process's refresh before refreshing without
/// `credentials.lock`. A refresh round trip is normally well under 3 s.
const REFRESH_FILE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The last refresh token this process saw refused, and why. Backs up the
/// persisted refusal when writing it failed (keyring locked, disk full).
static REFUSED_REFRESH_TOKEN: Lazy<Mutex<Option<(String, RefreshFailure)>>> =
    Lazy::new(|| Mutex::new(None));

/// `at` of the refusal this process last logged, so it is logged once.
static LOGGED_REFUSAL_AT: AtomicI64 = AtomicI64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthLoadIssue {
    NoCredentials,
    CredentialStore(String),
    RefreshExpired,
    RefreshFailed(String),
    /// The server refused the stored refresh token for good. Nothing retries
    /// until `autter login` stores new credentials.
    LoginRequired(RefreshFailure),
}

impl AuthLoadIssue {
    pub fn is_login_required(&self) -> bool {
        matches!(self, Self::LoginRequired(_))
    }
}

impl std::fmt::Display for AuthLoadIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCredentials => write!(f, "no stored credentials"),
            Self::CredentialStore(error) => write!(f, "could not read credentials: {error}"),
            Self::RefreshExpired => write!(f, "refresh token expired"),
            Self::RefreshFailed(error) => write!(f, "access-token refresh failed: {error}"),
            Self::LoginRequired(failure) => {
                write!(f, "sign-in refused by the server: {}", failure.message())
            }
        }
    }
}

/// Attempt to load stored credentials and refresh if needed.
/// Returns None on any failure (not logged in, expired, refresh failed).
/// Uses in-process Mutex for thread safety during token refresh.
fn try_load_auth_token() -> Result<String, AuthLoadIssue> {
    let store = CredentialStore::new();
    let result = load_auth_token(&store);
    if let Err(AuthLoadIssue::LoginRequired(failure)) = &result {
        report_login_required(failure);
    }
    result
}

fn load_auth_token(store: &CredentialStore) -> Result<String, AuthLoadIssue> {
    let creds = load_refreshable_credentials(store)?;

    // Fast path: if access token is valid (with 5 min buffer), use it directly
    if !creds.is_access_token_expired(300) {
        return Ok(creds.access_token);
    }

    // Need to refresh - acquire mutex to prevent thundering herd within this process
    // A panic in unrelated upload code must never disable token refresh for the
    // rest of this long-lived daemon process.
    let _guard = REFRESH_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Device-flow refresh tokens rotate on every use. The git proxy and the
    // daemon are separate processes, so an in-process mutex is not enough:
    // two concurrent refreshes can make the server invalidate the only copy
    // we still have on disk. Wait for a sibling refresh, then reload.
    let _file_lock = acquire_refresh_file_lock();

    let client = OAuthClient::new();
    refresh_stored_credentials(store, |refresh_token| {
        client.refresh_access_token(refresh_token)
    })
    // Mutex / file-lock guards are released when dropped
}

/// Load credentials that may be used or refreshed. A login the server already
/// refused, or one whose refresh token has expired, never reaches the network.
fn load_refreshable_credentials(
    store: &CredentialStore,
) -> Result<StoredCredentials, AuthLoadIssue> {
    let creds = match store.load() {
        Ok(Some(c)) => c,
        Ok(None) => return Err(AuthLoadIssue::NoCredentials),
        Err(error) => return Err(AuthLoadIssue::CredentialStore(error)),
    };
    if let Some(failure) = creds
        .refresh_failed
        .clone()
        .or_else(|| refused_in_process(&creds.refresh_token))
    {
        return Err(AuthLoadIssue::LoginRequired(failure));
    }
    if creds.is_refresh_token_expired() {
        return Err(AuthLoadIssue::RefreshExpired);
    }
    Ok(creds)
}

/// The refresh itself, run under the refresh locks. `refresh` performs the
/// network call (injected so tests can count calls without a server).
fn refresh_stored_credentials(
    store: &CredentialStore,
    refresh: impl Fn(&str) -> Result<StoredCredentials, TokenRequestError>,
) -> Result<String, AuthLoadIssue> {
    // Re-check credentials after acquiring lock - another thread/process may have refreshed
    let mut creds = load_refreshable_credentials(store)?;

    // Check again if access token is now valid (another thread/process may have refreshed)
    if !creds.is_access_token_expired(300) {
        return Ok(creds.access_token);
    }

    // Still expired - we need to refresh. At most one retry, with a token a
    // sibling process rotated in while this one was refreshing.
    let mut retried = false;
    loop {
        let error = match refresh(&creds.refresh_token) {
            Ok(new_creds) => {
                persist_refreshed_credentials(store, &creds.refresh_token, &new_creds);
                return Ok(new_creds.access_token);
            }
            Err(error) => error,
        };

        // Lost the rotation race: another process already swapped the
        // stored refresh token. Reload and use that session if it is live.
        if let Ok(Some(reloaded)) = store.load()
            && !reloaded.is_login_required()
            && !reloaded.is_refresh_token_expired()
        {
            if !reloaded.is_access_token_expired(300) {
                return Ok(reloaded.access_token);
            }
            if !retried && reloaded.refresh_token != creds.refresh_token {
                retried = true;
                creds = reloaded;
                continue;
            }
        }

        // A device token the server refused for good: stop here. PATs share
        // the refresh grant but are not rotated device sessions, so they keep
        // today's retry behavior.
        if error.requires_login()
            && !creds.refresh_token.starts_with(PAT_PREFIX)
            && let Some(oauth_error) = error.oauth_error()
            && let Some(failure) = record_refused_login(
                store,
                &creds.refresh_token,
                RefreshFailure::from_oauth_error(oauth_error),
            )
        {
            return Err(AuthLoadIssue::LoginRequired(failure));
        }

        // Transient (network, 5xx, temporarily_unavailable, access_denied):
        // keep the credentials and retry on a later call.
        tracing::warn!(%error, "could not refresh autter access token");
        return Err(AuthLoadIssue::RefreshFailed(format!(
            "Token refresh failed: {error}"
        )));
    }
}

/// Write rotated credentials unless the store has moved on (compare-and-set
/// on the refresh token that was sent). The current process can use the fresh
/// token even if persisting it fails, but surface that failure so the next
/// process is diagnosable.
fn persist_refreshed_credentials(
    store: &CredentialStore,
    sent_refresh_token: &str,
    new_creds: &StoredCredentials,
) {
    match store.store_if_current(sent_refresh_token, new_creds) {
        Ok(true) => {}
        Ok(false) => tracing::info!(
            "stored credentials changed during refresh; keeping the newer stored session"
        ),
        Err(error) => tracing::warn!(%error, "could not persist refreshed credentials"),
    }
}

/// Persist the refusal with the credentials so every process stops refreshing
/// them. If that write fails, this process remembers it instead, so the loop
/// cannot restart. Returns `None` when the store already holds other
/// credentials (a sibling refresh or `autter login` landed meanwhile): those
/// are not refused.
fn record_refused_login(
    store: &CredentialStore,
    refresh_token: &str,
    failure: RefreshFailure,
) -> Option<RefreshFailure> {
    match store.mark_refresh_failed(refresh_token, failure.clone()) {
        Ok(recorded) => recorded,
        Err(error) => {
            tracing::warn!(%error, "could not persist the refused login; remembering it in this process");
            if let Ok(mut refused) = REFUSED_REFRESH_TOKEN.lock() {
                *refused = Some((refresh_token.to_string(), failure.clone()));
            }
            Some(failure)
        }
    }
}

fn refused_in_process(refresh_token: &str) -> Option<RefreshFailure> {
    let refused = REFUSED_REFRESH_TOKEN.lock().ok()?;
    match refused.as_ref() {
        Some((token, failure)) if token == refresh_token => Some(failure.clone()),
        _ => None,
    }
}

/// Log a refused login once per process (the daemon would otherwise log it on
/// every flush tick) and tell an interactive user once.
fn report_login_required(failure: &RefreshFailure) {
    if first_report_in_process(failure) {
        tracing::warn!(
            error = %failure.error,
            error_code = failure.error_code.as_deref().unwrap_or("none"),
            reason = %failure.message(),
            "autter login refused by the server; not refreshing again until `autter login` stores new credentials"
        );
    }
    crate::auth::notice::announce_login_refused(failure);
}

/// True the first time this process reports `failure`.
fn first_report_in_process(failure: &RefreshFailure) -> bool {
    LOGGED_REFUSAL_AT.swap(failure.at, Ordering::Relaxed) != failure.at
}

/// Cross-process lock around refresh-token rotation.
///
/// Returns `None` in tests, when the lock file cannot be created, or if a
/// sibling still holds the lock after [`REFRESH_FILE_LOCK_WAIT`]. The caller
/// then refreshes without it; the compare-and-set write keeps that safe.
fn acquire_refresh_file_lock() -> Option<crate::utils::LockFile> {
    if cfg!(test) || std::env::var_os("AUTTER_TEST_DB_PATH").is_some() {
        return None;
    }
    let path = crate::mdm::utils::home_dir()
        .join(".autter")
        .join("internal")
        .join("credentials.lock");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let deadline = std::time::Instant::now() + REFRESH_FILE_LOCK_WAIT;
    loop {
        if let Some(lock) = crate::utils::LockFile::try_acquire(&path) {
            return Some(lock);
        }
        if std::time::Instant::now() >= deadline {
            tracing::debug!("credentials.lock still held; refreshing without it");
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Prefix marking a Personal Access Token (mirrors the backend).
const PAT_PREFIX: &str = "autter_pat_";

/// In-process cache: repo remote URL → resolved owning org id (None = untracked).
static ORG_RESOLVE_CACHE: Lazy<Mutex<std::collections::HashMap<String, Option<String>>>> =
    Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// In-process cache: org id → (access token, unix expiry).
static ORG_TOKEN_CACHE: Lazy<Mutex<std::collections::HashMap<String, (String, i64)>>> =
    Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// Load the stored Personal Access Token (the refresh-token slot), if the user is
/// logged in with a PAT. Per-org routing only works with a PAT; device refresh
/// tokens return None so callers fall back to the single home-org token.
fn load_stored_pat() -> Option<String> {
    let creds = CredentialStore::new().load().ok().flatten()?;
    if creds.is_refresh_token_expired() {
        return None;
    }
    if !creds.refresh_token.starts_with(PAT_PREFIX) {
        return None;
    }
    Some(creds.refresh_token)
}

/// Resolve which org owns the repo at `repo_url`, cached in-process. Returns None
/// when no org tracks it, the user isn't logged in with a PAT, or resolution fails
/// — callers then fall back to the home org.
pub fn resolve_org_for_repo_cached(repo_url: &str) -> Option<String> {
    if let Ok(cache) = ORG_RESOLVE_CACHE.lock()
        && let Some(hit) = cache.get(repo_url)
    {
        return hit.clone();
    }
    let pat = load_stored_pat()?;
    let resolved = OAuthClient::new()
        .resolve_org_for_repo(&pat, repo_url)
        .ok()
        .flatten();
    if let Ok(mut cache) = ORG_RESOLVE_CACHE.lock() {
        cache.insert(repo_url.to_string(), resolved.clone());
    }
    resolved
}

/// Get an access token scoped to `org_id`, minted from the stored PAT and cached
/// in-process until near expiry. Returns None if not logged in with a PAT or the
/// mint fails (caller should NOT fall back to another org — retry instead).
pub fn access_token_for_org(org_id: &str) -> Option<String> {
    let now = chrono::Utc::now().timestamp();
    if let Ok(cache) = ORG_TOKEN_CACHE.lock()
        && let Some((token, exp)) = cache.get(org_id)
        && *exp > now + 300
    {
        return Some(token.clone());
    }
    let pat = load_stored_pat()?;
    let creds = OAuthClient::new()
        .exchange_pat_for_org(&pat, Some(org_id))
        .ok()?;
    if let Ok(mut cache) = ORG_TOKEN_CACHE.lock() {
        cache.insert(
            org_id.to_string(),
            (creds.access_token.clone(), creds.access_token_expires_at),
        );
    }
    Some(creds.access_token)
}

/// Resolve the autter effective author identity without requiring a Repository instance.
///
/// Uses the shared git identity helper to get the current user's identity,
/// respecting the full git precedence chain (env vars > config > system defaults),
/// then overlays any configured autter author fields.
/// Falls back to the system hostname if git identity is unavailable.
fn resolve_git_identity() -> Option<String> {
    let author_config = config::Config::fresh_author_cached();
    let identity = current_git_committer_identity_resolution()
        .identity
        .with_author_config(&author_config);
    if let Some(formatted) = identity.formatted() {
        return Some(encode_for_header(&formatted));
    }

    resolve_fallback_identity()
        .map(|id| parse_git_var_identity(&id).with_author_config(&author_config))
        .and_then(|identity| identity.formatted())
        .map(|id| encode_for_header(&id))
}

/// Build a fallback identity matching git's format: `"Username <username@hostname>"`.
fn resolve_fallback_identity() -> Option<String> {
    let username = resolve_username()?;
    let hostname = resolve_hostname().unwrap_or_else(|| "localhost".to_string());
    Some(format!("{} <{}@{}>", username, username, hostname))
}

fn resolve_username() -> Option<String> {
    #[cfg(windows)]
    if let Ok(u) = std::env::var("USERNAME")
        && !u.trim().is_empty()
    {
        return Some(u.trim().to_string());
    }
    #[cfg(not(windows))]
    if let Ok(u) = std::env::var("USER")
        && !u.trim().is_empty()
    {
        return Some(u.trim().to_string());
    }
    None
}

pub(crate) fn resolve_hostname() -> Option<String> {
    #[cfg(windows)]
    if let Ok(h) = std::env::var("COMPUTERNAME")
        && !h.trim().is_empty()
    {
        return Some(h.trim().to_string());
    }
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.trim().is_empty()
    {
        return Some(h.trim().to_string());
    }
    let mut cmd = std::process::Command::new("hostname");
    #[cfg(windows)]
    {
        use crate::utils::CREATE_NO_WINDOW;
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NO_WINDOW);
    }
    let output = cmd.output().ok()?;
    let h = String::from_utf8(output.stdout).ok()?;
    let h = h.trim();
    if h.is_empty() {
        None
    } else {
        Some(h.to_string())
    }
}

/// Percent-encode non-ASCII and control bytes so the value is safe for HTTP headers.
/// ureq 2.x accepts only visible ASCII (0x21..=0x7E) and space/tab in header values.
fn encode_for_header(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'%' => encoded.push_str("%25"),
            0x20..=0x7E => encoded.push(byte as char),
            _ => write!(encoded, "%{:02X}", byte).unwrap(),
        }
    }
    encoded
}

/// API client context with optional authentication
#[derive(Clone)]
pub struct ApiContext {
    /// Base URL for the API (e.g., `https://app.com`)
    pub base_url: String,
    /// Optional authentication token
    pub auth_token: Option<String>,
    /// Why automatic credential loading did not produce an access token.
    pub auth_issue: Option<AuthLoadIssue>,
    /// Optional API key for X-API-Key header
    pub api_key: Option<String>,
    /// Optional git author identity for X-Author-Identity header (only sent when API key is set)
    pub author_identity: Option<String>,
    /// Request timeout in seconds
    pub timeout_secs: Option<u64>,
}

impl std::fmt::Debug for ApiContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiContext")
            .field("base_url", &self.base_url)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("auth_issue", &self.auth_issue)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("author_identity", &self.author_identity)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl ApiContext {
    /// Get the default API base URL from config
    /// Uses Config::fresh() to support runtime config updates (daemon mode)
    fn default_base_url() -> String {
        config::Config::fresh().api_base_url().to_string()
    }

    /// Create a GET request with common headers (User-Agent, X-Distinct-ID)
    /// Use this for all HTTP GET requests to ensure consistent headers.
    /// The returned (Agent, Request) pair uses the system's native certificate store.
    pub fn http_get(url: &str, timeout_secs: Option<u64>) -> (ureq::Agent, ureq::Request) {
        let agent = http::build_agent(timeout_secs);
        let request = agent
            .get(url)
            .set(
                "User-Agent",
                &format!("autter/{}", env!("CARGO_PKG_VERSION")),
            )
            .set("X-Distinct-ID", &config::get_or_create_distinct_id());
        (agent, request)
    }

    /// Create a POST request with common headers (User-Agent, X-Distinct-ID)
    /// Use this for all HTTP POST requests to ensure consistent headers.
    /// The returned (Agent, Request) pair uses the system's native certificate store.
    pub fn http_post(url: &str, timeout_secs: Option<u64>) -> (ureq::Agent, ureq::Request) {
        let agent = http::build_agent(timeout_secs);
        let request = agent
            .post(url)
            .set(
                "User-Agent",
                &format!("autter/{}", env!("CARGO_PKG_VERSION")),
            )
            .set("X-Distinct-ID", &config::get_or_create_distinct_id());
        (agent, request)
    }

    /// Create a new API context, automatically using stored credentials if available
    /// If base_url is None, uses api_base_url from config (which can be set via config file, env var, or defaults)
    /// Uses Config::fresh() to support runtime config updates (daemon mode)
    pub fn new(base_url: Option<String>) -> Self {
        let cfg = config::Config::fresh();
        let api_key = cfg.api_key().map(|s| s.to_string());
        let author_identity = if api_key.is_some() {
            resolve_git_identity()
        } else {
            None
        };
        let (auth_token, auth_issue) = match try_load_auth_token() {
            Ok(token) => (Some(token), None),
            Err(issue) => (None, Some(issue)),
        };
        Self {
            base_url: base_url.unwrap_or_else(Self::default_base_url),
            auth_token,
            auth_issue,
            api_key,
            author_identity,
            timeout_secs: Some(30),
        }
    }

    /// Create a new API context explicitly without authentication
    /// Use this when you need to ensure no auth token is sent
    /// Uses Config::fresh() to support runtime config updates (daemon mode)
    #[allow(dead_code)]
    pub fn without_auth(base_url: Option<String>) -> Self {
        let cfg = config::Config::fresh();
        let api_key = cfg.api_key().map(|s| s.to_string());
        let author_identity = if api_key.is_some() {
            resolve_git_identity()
        } else {
            None
        };
        Self {
            base_url: base_url.unwrap_or_else(Self::default_base_url),
            auth_token: None,
            auth_issue: None,
            api_key,
            author_identity,
            timeout_secs: Some(30),
        }
    }

    /// Create a new API context with authentication
    /// If base_url is None, uses api_base_url from config (which can be set via config file, env var, or defaults)
    /// Uses Config::fresh() to support runtime config updates (daemon mode)
    #[allow(dead_code)]
    pub fn with_auth(base_url: Option<String>, auth_token: String) -> Self {
        let cfg = config::Config::fresh();
        let api_key = cfg.api_key().map(|s| s.to_string());
        let author_identity = if api_key.is_some() {
            resolve_git_identity()
        } else {
            None
        };
        Self {
            base_url: base_url.unwrap_or_else(Self::default_base_url),
            auth_token: Some(auth_token),
            auth_issue: None,
            api_key,
            author_identity,
            timeout_secs: Some(30),
        }
    }

    /// Set a custom timeout
    pub fn with_timeout(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = Some(timeout_secs);
        self
    }

    /// Build the full URL for an endpoint.
    ///
    /// The endpoint is appended to the base URL preserving any path prefix on
    /// the base — i.e. `https://host/api/autter` + `/worker/notes/upload`
    /// yields `https://host/api/autter/worker/notes/upload`. Leading/trailing
    /// slashes are normalized so the join works regardless of which side
    /// carries the separator.
    fn build_url(&self, endpoint: &str) -> Result<String, AutterError> {
        Url::parse(&self.base_url)
            .map_err(|e| AutterError::Generic(format!("Invalid base URL: {}", e)))?;
        let joined = format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            endpoint.trim_start_matches('/')
        );
        Url::parse(&joined)
            .map_err(|e| AutterError::Generic(format!("Invalid endpoint URL: {}", e)))?;
        Ok(joined)
    }

    /// Make a POST request with JSON body
    pub fn post_json<T: serde::Serialize>(
        &self,
        endpoint: &str,
        body: &T,
    ) -> Result<http::Response, AutterError> {
        let url = self.build_url(endpoint)?;
        let body_json = serde_json::to_string(body).map_err(AutterError::JsonError)?;

        let (_agent, mut request) = Self::http_post(&url, self.timeout_secs);
        request = request.set("Content-Type", "application/json");

        if let Some(api_key) = &self.api_key {
            request = request.set("X-API-Key", api_key);
            if let Some(identity) = &self.author_identity {
                request = request.set("X-Author-Identity", identity);
            }
        }
        if let Some(token) = &self.auth_token {
            request = request.set("Authorization", &format!("Bearer {}", token));
        }

        http::send_with_body(request, &body_json)
            .map_err(|e| AutterError::Generic(format!("HTTP request failed: {}", e)))
    }

    /// Make a GET request
    pub fn get(&self, endpoint: &str) -> Result<http::Response, AutterError> {
        let url = self.build_url(endpoint)?;

        let (_agent, mut request) = Self::http_get(&url, self.timeout_secs);

        if let Some(api_key) = &self.api_key {
            request = request.set("X-API-Key", api_key);
            if let Some(identity) = &self.author_identity {
                request = request.set("X-Author-Identity", identity);
            }
        }
        if let Some(token) = &self.auth_token {
            request = request.set("Authorization", &format!("Bearer {}", token));
        }

        http::send(request).map_err(|e| AutterError::Generic(format!("HTTP request failed: {}", e)))
    }
}

/// API client wrapper
#[derive(Debug, Clone)]
pub struct ApiClient {
    context: ApiContext,
}

impl ApiClient {
    /// Create a new API client with the given context
    pub fn new(context: ApiContext) -> Self {
        Self { context }
    }

    /// Get a reference to the API context
    pub fn context(&self) -> &ApiContext {
        &self.context
    }

    /// Get a mutable reference to the API context
    #[allow(dead_code)]
    pub fn context_mut(&mut self) -> &mut ApiContext {
        &mut self.context
    }

    /// Check if user is logged in (has an auth token)
    pub fn is_logged_in(&self) -> bool {
        self.context.auth_token.is_some()
    }

    pub fn auth_issue(&self) -> Option<&AuthLoadIssue> {
        self.context.auth_issue.as_ref()
    }

    /// The auth issue worth a log line on each flush. A refused login is left
    /// out: the auth loader logs it once, and it cannot change before `autter login`.
    pub fn auth_issue_to_log(&self) -> Option<&AuthLoadIssue> {
        self.auth_issue().filter(|issue| !issue.is_login_required())
    }

    /// Check if an API key is configured
    pub fn has_api_key(&self) -> bool {
        self.context.api_key.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ============= ApiContext Tests =============

    #[test]
    fn test_api_context_without_auth() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        assert!(ctx.auth_token.is_none());
        assert_eq!(ctx.base_url, "https://example.com");
    }

    #[test]
    fn test_api_context_with_auth() {
        let ctx = ApiContext::with_auth(
            Some("https://example.com".to_string()),
            "test_token".to_string(),
        );
        assert_eq!(ctx.auth_token, Some("test_token".to_string()));
        assert_eq!(ctx.base_url, "https://example.com");
    }

    #[test]
    fn test_api_context_with_timeout() {
        let ctx =
            ApiContext::without_auth(Some("https://example.com".to_string())).with_timeout(60);
        assert_eq!(ctx.timeout_secs, Some(60));
    }

    #[test]
    fn test_api_context_default_timeout() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        assert_eq!(ctx.timeout_secs, Some(30));
    }

    // ============= ApiClient Tests =============

    #[test]
    fn test_api_client_is_logged_in_true() {
        let ctx =
            ApiContext::with_auth(Some("https://example.com".to_string()), "token".to_string());
        let client = ApiClient::new(ctx);
        assert!(client.is_logged_in());
    }

    #[test]
    fn test_api_client_is_logged_in_false() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        let client = ApiClient::new(ctx);
        assert!(!client.is_logged_in());
    }

    #[test]
    fn test_api_client_context_access() {
        let ctx =
            ApiContext::with_auth(Some("https://example.com".to_string()), "token".to_string());
        let client = ApiClient::new(ctx);
        assert_eq!(client.context().base_url, "https://example.com");
    }

    // ============= URL Building Tests =============

    #[test]
    fn test_build_url_simple() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        let url = ctx.build_url("/api/test").unwrap();
        assert_eq!(url, "https://example.com/api/test");
    }

    #[test]
    fn test_build_url_with_trailing_slash() {
        let ctx = ApiContext::without_auth(Some("https://example.com/".to_string()));
        let url = ctx.build_url("api/test").unwrap();
        assert_eq!(url, "https://example.com/api/test");
    }

    #[test]
    fn test_build_url_invalid_base() {
        let ctx = ApiContext::without_auth(Some("not-a-url".to_string()));
        let result = ctx.build_url("/api/test");
        assert!(result.is_err());
    }

    #[test]
    fn test_build_url_preserves_path_prefix() {
        let ctx = ApiContext::without_auth(Some("https://example.com/api/autter".to_string()));
        let url = ctx.build_url("/worker/notes/upload").unwrap();
        assert_eq!(url, "https://example.com/api/autter/worker/notes/upload");
    }

    #[test]
    fn test_build_url_preserves_path_prefix_with_trailing_slash() {
        let ctx = ApiContext::without_auth(Some("https://example.com/api/autter/".to_string()));
        let url = ctx.build_url("/worker/notes/upload").unwrap();
        assert_eq!(url, "https://example.com/api/autter/worker/notes/upload");
    }

    #[test]
    fn test_build_url_preserves_query_string() {
        let ctx = ApiContext::without_auth(Some("https://example.com/api/autter".to_string()));
        let url = ctx.build_url("/worker/notes/?commits=abc,def").unwrap();
        assert_eq!(
            url,
            "https://example.com/api/autter/worker/notes/?commits=abc,def"
        );
    }

    // ============= Mutex Thread Safety Tests =============

    #[test]
    fn test_mutex_is_accessible() {
        // Simple test to verify the mutex can be locked
        let guard = REFRESH_LOCK.lock();
        assert!(guard.is_ok());
        // Guard drops here, releasing the lock
    }

    #[test]
    fn test_concurrent_access_to_mutex() {
        // Test that multiple threads can safely contend for the mutex
        let counter = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];

        for _ in 0..5 {
            let counter_clone = counter.clone();
            let handle = std::thread::spawn(move || {
                if let Ok(_guard) = REFRESH_LOCK.lock() {
                    counter_clone.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // All threads should have acquired the lock sequentially
        let final_count = counter.load(Ordering::SeqCst);
        assert_eq!(final_count, 5);
    }

    // ============= Dead refresh token (CE-20) =============

    use crate::auth::credential_backend::{CredentialBackend, MockBackend};
    use crate::auth::types::OAuthError;
    use std::cell::Cell;

    fn creds(refresh_token: &str, access_token: &str, access_expires_in: i64) -> StoredCredentials {
        let now = chrono::Utc::now().timestamp();
        StoredCredentials {
            access_token: access_token.to_string(),
            refresh_token: refresh_token.to_string(),
            access_token_expires_at: now + access_expires_in,
            refresh_token_expires_at: now + 86400 * 90,
            refresh_failed: None,
        }
    }

    /// Stored credentials whose access token has expired, so the next load refreshes.
    fn store_needing_refresh(refresh_token: &str) -> CredentialStore {
        let store = CredentialStore::with_backend(Box::new(MockBackend::new()));
        store
            .store(&creds(refresh_token, "expired-access", -60))
            .unwrap();
        store
    }

    fn refusal(status: u16, json: &str) -> TokenRequestError {
        TokenRequestError::Refused {
            status,
            error: serde_json::from_str::<OAuthError>(json).unwrap(),
        }
    }

    const DEAD_TOKEN_BODY: &str = r#"{"error":"invalid_grant","error_description":"Refresh token is not valid. Run `autter login` to sign in again.","error_code":"refresh_token_invalid","login_required":true}"#;

    #[test]
    fn dead_refresh_token_is_not_refreshed_again_until_login() {
        let store = store_needing_refresh("rt-ce20-dead");
        let calls = Cell::new(0);
        let refresh = |token: &str| {
            calls.set(calls.get() + 1);
            if token == "rt-ce20-fresh" {
                Ok(creds("rt-ce20-fresh-2", "fresh-access", 3600))
            } else {
                Err(refusal(400, DEAD_TOKEN_BODY))
            }
        };

        let Err(AuthLoadIssue::LoginRequired(failure)) =
            refresh_stored_credentials(&store, refresh)
        else {
            panic!("a dead token must end in LoginRequired");
        };
        assert_eq!(calls.get(), 1);
        assert_eq!(failure.error_code.as_deref(), Some("refresh_token_invalid"));
        assert!(failure.message().contains("autter login"));
        // The refusal is persisted with the credentials for every process.
        assert_eq!(
            store.load().unwrap().unwrap().refresh_failed,
            Some(failure.clone())
        );

        // Every later attempt (daemon ticks, other commands) stops before the network.
        for _ in 0..5 {
            assert_eq!(
                refresh_stored_credentials(&store, refresh),
                Err(AuthLoadIssue::LoginRequired(failure.clone()))
            );
            assert_eq!(
                load_refreshable_credentials(&store).unwrap_err(),
                AuthLoadIssue::LoginRequired(failure.clone())
            );
        }
        assert_eq!(calls.get(), 1);

        // `autter login` writes new credentials; refreshing resumes.
        store
            .store(&creds("rt-ce20-fresh", "login-access", -60))
            .unwrap();
        assert_eq!(
            refresh_stored_credentials(&store, refresh),
            Ok("fresh-access".to_string())
        );
        assert_eq!(calls.get(), 2);
        let stored = store.load().unwrap().unwrap();
        assert_eq!(stored.refresh_token, "rt-ce20-fresh-2");
        assert!(!stored.is_login_required());
    }

    #[test]
    fn refused_credentials_skip_even_a_valid_access_token() {
        let store = CredentialStore::with_backend(Box::new(MockBackend::new()));
        let mut stored = creds("rt-ce20-refused-live", "still-valid", 3600);
        stored.refresh_failed = Some(RefreshFailure::from_oauth_error(
            &serde_json::from_str(DEAD_TOKEN_BODY).unwrap(),
        ));
        store.store(&stored).unwrap();

        assert!(matches!(
            load_auth_token(&store),
            Err(AuthLoadIssue::LoginRequired(_))
        ));
    }

    #[test]
    fn older_server_invalid_grant_is_terminal_too() {
        let store = store_needing_refresh("rt-ce20-old-server");
        let calls = Cell::new(0);
        let refresh = |_: &str| {
            calls.set(calls.get() + 1);
            Err(refusal(400, r#"{"error":"invalid_grant"}"#))
        };
        assert!(matches!(
            refresh_stored_credentials(&store, refresh),
            Err(AuthLoadIssue::LoginRequired(_))
        ));
        assert!(matches!(
            refresh_stored_credentials(&store, refresh),
            Err(AuthLoadIssue::LoginRequired(_))
        ));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn transient_refresh_errors_are_retried() {
        let transient = [
            TokenRequestError::Transport("Failed to connect to server: timed out".to_string()),
            TokenRequestError::Transport("HTTP 502: <html>Bad Gateway</html>".to_string()),
            refusal(503, r#"{"error":"temporarily_unavailable"}"#),
            refusal(400, r#"{"error":"temporarily_unavailable"}"#),
            refusal(400, r#"{"error":"access_denied"}"#),
            refusal(500, r#"{"error":"server_error"}"#),
        ];
        for (index, error) in transient.into_iter().enumerate() {
            let store = store_needing_refresh(&format!("rt-ce20-transient-{index}"));
            let calls = Cell::new(0);
            let refresh = |_: &str| {
                calls.set(calls.get() + 1);
                Err(error.clone())
            };
            for _ in 0..2 {
                assert!(matches!(
                    refresh_stored_credentials(&store, refresh),
                    Err(AuthLoadIssue::RefreshFailed(_))
                ));
            }
            assert_eq!(calls.get(), 2, "{error:?} must be retried");
            assert!(!store.load().unwrap().unwrap().is_login_required());
        }
    }

    #[test]
    fn refused_pat_is_not_marked_dead() {
        let store = store_needing_refresh("autter_pat_ce20");
        let calls = Cell::new(0);
        let refresh = |_: &str| {
            calls.set(calls.get() + 1);
            Err(refusal(400, DEAD_TOKEN_BODY))
        };
        for _ in 0..2 {
            assert!(matches!(
                refresh_stored_credentials(&store, refresh),
                Err(AuthLoadIssue::RefreshFailed(_))
            ));
        }
        assert_eq!(calls.get(), 2);
        assert!(!store.load().unwrap().unwrap().is_login_required());
    }

    #[test]
    fn superseded_by_a_sibling_uses_the_sibling_session() {
        // Another process rotated rt-1 -> rt-2 while this one was refreshing
        // rt-1; the server calls rt-1 superseded. That is not a dead login.
        let store = store_needing_refresh("rt-ce20-sibling-1");
        let refresh = |token: &str| {
            assert_eq!(token, "rt-ce20-sibling-1");
            store
                .store(&creds("rt-ce20-sibling-2", "sibling-access", 3600))
                .unwrap();
            Err(refusal(
                400,
                r#"{"error":"invalid_grant","error_code":"refresh_token_superseded","login_required":true}"#,
            ))
        };
        assert_eq!(
            refresh_stored_credentials(&store, refresh),
            Ok("sibling-access".to_string())
        );
        let stored = store.load().unwrap().unwrap();
        assert_eq!(stored.refresh_token, "rt-ce20-sibling-2");
        assert!(!stored.is_login_required());
    }

    #[test]
    fn refreshed_credentials_do_not_overwrite_a_concurrent_login() {
        let store = store_needing_refresh("rt-ce20-cas-1");
        let refresh = |_: &str| {
            // `autter login` lands while this refresh is in flight.
            store
                .store(&creds("rt-ce20-cas-login", "login-access", 3600))
                .unwrap();
            Ok(creds("rt-ce20-cas-2", "refreshed-access", 3600))
        };
        assert_eq!(
            refresh_stored_credentials(&store, refresh),
            Ok("refreshed-access".to_string())
        );
        assert_eq!(
            store.load().unwrap().unwrap().refresh_token,
            "rt-ce20-cas-login"
        );
    }

    #[test]
    fn refusal_is_remembered_when_it_cannot_be_persisted() {
        let backend = MockBackend::new();
        backend
            .store(&serde_json::to_string(&creds("rt-ce20-disk-full", "expired", -60)).unwrap())
            .unwrap();
        let store = CredentialStore::with_backend(Box::new(backend.fail_store("disk full")));
        let calls = Cell::new(0);
        let refresh = |_: &str| {
            calls.set(calls.get() + 1);
            Err(refusal(400, DEAD_TOKEN_BODY))
        };
        for _ in 0..3 {
            assert!(matches!(
                refresh_stored_credentials(&store, refresh),
                Err(AuthLoadIssue::LoginRequired(_))
            ));
        }
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn a_refusal_is_logged_once_per_process() {
        let failure = RefreshFailure {
            at: 1_760_000_123,
            error: "invalid_grant".to_string(),
            error_code: None,
            description: None,
        };
        assert!(first_report_in_process(&failure));
        assert!(!first_report_in_process(&failure));
        assert!(!first_report_in_process(&failure));
        // A new login that dies again is a new refusal.
        let later = RefreshFailure {
            at: failure.at + 1,
            ..failure
        };
        assert!(first_report_in_process(&later));
    }

    #[test]
    fn login_required_is_not_logged_per_flush() {
        let refused = ApiClient::new(ApiContext {
            auth_issue: Some(AuthLoadIssue::LoginRequired(RefreshFailure {
                at: 1,
                error: "invalid_grant".to_string(),
                error_code: None,
                description: None,
            })),
            ..ApiContext::without_auth(Some("https://example.com".to_string()))
        });
        assert!(refused.auth_issue().is_some());
        assert!(refused.auth_issue_to_log().is_none());

        let transient = ApiClient::new(ApiContext {
            auth_issue: Some(AuthLoadIssue::RefreshFailed("timed out".to_string())),
            ..ApiContext::without_auth(Some("https://example.com".to_string()))
        });
        assert!(transient.auth_issue_to_log().is_some());
    }

    // ============= encode_for_header Tests =============

    #[test]
    fn test_encode_for_header_ascii_passthrough() {
        let value = "John Doe <john@example.com>";
        assert_eq!(encode_for_header(value), value);
    }

    #[test]
    fn test_encode_for_header_non_ascii() {
        assert_eq!(
            encode_for_header("Ex\u{00f6}utf8lastname <user@example.com>"),
            "Ex%C3%B6utf8lastname <user@example.com>"
        );
    }

    #[test]
    fn test_encode_for_header_percent_encoded_for_reversibility() {
        assert_eq!(encode_for_header("100% done"), "100%25 done");
    }

    #[test]
    fn test_encode_for_header_special_ascii_chars_passthrough() {
        let value = "Name+Tag <user+tag@sub.example.com>";
        assert_eq!(encode_for_header(value), value);
    }

    #[test]
    fn test_encode_for_header_all_bytes_valid_for_ureq() {
        let input = "Ñoño García <nono@example.com>";
        let encoded = encode_for_header(input);
        assert!(
            encoded
                .bytes()
                .all(|b| b == b' ' || b == b'\t' || (0x21..=0x7E).contains(&b)),
            "encoded value contains invalid header bytes: {:?}",
            encoded
        );
    }
}
