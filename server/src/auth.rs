//! Better Auth identity verification.
//!
//! autter.dev runs Better Auth and is the identity/control plane. Its JWT plugin
//! signs tokens and publishes a JWKS endpoint. The CLI sends one of those tokens
//! as `Authorization: Bearer <jwt>`; here we verify the signature against the
//! JWKS (stateless — no per-request call to autter.dev) and read the claims.
//!
//! The JWT payload must include these custom claims (configure Better Auth's JWT
//! `definePayload` accordingly):
//! - `org_id`     — the caller's organization id
//! - `org_db_url` — the Postgres URL of that organization's database
//!
//! Standard claims used: `sub` (user id), `email`, `name`, `exp`.
//!
//! Dev mode (`AUTTER_SERVER_DEV_AUTH=1`) decodes the token WITHOUT verifying the
//! signature, so the data path can be exercised with a hand-crafted JWT before
//! autter.dev is reachable. Never enable it in production.

use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::error::AppError;

/// JWKS are refreshed at most this often (and on an unknown `kid`).
const JWKS_TTL: Duration = Duration::from_secs(3600);

/// Claims we read out of the Better Auth JWT.
#[derive(Debug, Deserialize)]
struct Claims {
    sub: Option<String>,
    email: Option<String>,
    #[allow(dead_code)]
    name: Option<String>,
    org_id: Option<String>,
    org_db_url: Option<String>,
    // `exp` is validated by jsonwebtoken; we don't need to read it here.
}

/// The authenticated caller and where their data lives.
pub struct Identity {
    pub user_id: Option<String>,
    #[allow(dead_code)] // surfaced for future logging/attribution
    pub email: Option<String>,
    #[allow(dead_code)] // tenant is selected via org_db_url; org_id kept for logs
    pub org_id: String,
    pub org_db_url: String,
    pub distinct_id: Option<String>,
}

pub struct JwtVerifier {
    jwks_url: Option<String>,
    issuer: Option<String>,
    audience: Option<String>,
    dev_no_verify: bool,
    http: reqwest::Client,
    cache: RwLock<Option<(Instant, JwkSet)>>,
}

impl JwtVerifier {
    pub fn new(
        jwks_url: Option<String>,
        issuer: Option<String>,
        audience: Option<String>,
        dev_no_verify: bool,
    ) -> Self {
        Self {
            jwks_url,
            issuer,
            audience,
            dev_no_verify,
            http: reqwest::Client::new(),
            cache: RwLock::new(None),
        }
    }

    /// Verify the request's bearer token and return the caller's identity.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<Identity, AppError> {
        let token = extract_bearer(headers)
            .ok_or_else(|| AppError::Unauthorized("Missing bearer token".to_string()))?;

        let claims = if self.dev_no_verify {
            decode_claims_unverified(&token)?
        } else {
            self.verify(&token).await?
        };

        let org_id = claims
            .org_id
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::Unauthorized("Token missing org_id claim".to_string()))?;
        let org_db_url = claims
            .org_db_url
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::Unauthorized("Token missing org_db_url claim".to_string()))?;

        let distinct_id = headers
            .get("x-distinct-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        Ok(Identity {
            user_id: claims.sub,
            email: claims.email,
            org_id,
            org_db_url,
            distinct_id,
        })
    }

    /// Verify a token's signature against the JWKS and return its claims.
    async fn verify(&self, token: &str) -> Result<Claims, AppError> {
        let header = decode_header(token)
            .map_err(|e| AppError::Unauthorized(format!("Invalid token header: {e}")))?;
        let kid = header
            .kid
            .clone()
            .ok_or_else(|| AppError::Unauthorized("Token missing kid".to_string()))?;

        let jwk = match self.find_key(&kid, false).await? {
            Some(jwk) => jwk,
            // Unknown kid: keys may have rotated — force a refresh and retry once.
            None => self
                .find_key(&kid, true)
                .await?
                .ok_or_else(|| AppError::Unauthorized("No matching signing key".to_string()))?,
        };

        let decoding_key = DecodingKey::from_jwk(&jwk)
            .map_err(|e| AppError::Internal(format!("bad JWKS key: {e}")))?;

        let mut validation = Validation::new(header.alg);
        validation.validate_exp = true;
        if let Some(iss) = &self.issuer {
            validation.set_issuer(&[iss]);
        }
        if let Some(aud) = &self.audience {
            validation.set_audience(&[aud]);
        } else {
            // No audience configured: don't require/validate the `aud` claim.
            validation.validate_aud = false;
        }

        let data = decode::<Claims>(token, &decoding_key, &validation)
            .map_err(|e| AppError::Unauthorized(format!("Token verification failed: {e}")))?;
        Ok(data.claims)
    }

    /// Look up a JWK by `kid`, optionally forcing a JWKS refresh first.
    async fn find_key(
        &self,
        kid: &str,
        force_refresh: bool,
    ) -> Result<Option<jsonwebtoken::jwk::Jwk>, AppError> {
        if !force_refresh {
            if let Some((fetched_at, jwks)) = self.cache.read().await.as_ref() {
                if fetched_at.elapsed() < JWKS_TTL {
                    return Ok(jwks.find(kid).cloned());
                }
            }
        }

        let jwks = self.fetch_jwks().await?;
        let found = jwks.find(kid).cloned();
        *self.cache.write().await = Some((Instant::now(), jwks));
        Ok(found)
    }

    async fn fetch_jwks(&self) -> Result<JwkSet, AppError> {
        let url = self
            .jwks_url
            .as_ref()
            .ok_or_else(|| AppError::Internal("AUTTER_SERVER_JWKS_URL not configured".to_string()))?;
        let jwks = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("JWKS fetch failed: {e}")))?
            .json::<JwkSet>()
            .await
            .map_err(|e| AppError::Internal(format!("JWKS parse failed: {e}")))?;
        Ok(jwks)
    }
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get("authorization")?.to_str().ok()?;
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))?
        .trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Decode the JWT payload WITHOUT verifying the signature (dev mode only).
fn decode_claims_unverified(token: &str) -> Result<Claims, AppError> {
    use base64::Engine;
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| AppError::Unauthorized("Malformed token".to_string()))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| AppError::Unauthorized(format!("Bad token payload: {e}")))?;
    serde_json::from_slice::<Claims>(&bytes)
        .map_err(|e| AppError::Unauthorized(format!("Bad token claims: {e}")))
}
