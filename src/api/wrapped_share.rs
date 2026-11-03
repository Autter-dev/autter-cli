//! `autter wrapped --share`: publish the year summary as a page at
//! `app.autter.dev/wrapped/<token>` with a share image, and take it down
//! again with `--unshare`. The request carries the exact `--json` document;
//! the server drops the prompt snippet unless the user opted in, and the
//! client never sends it otherwise either.

use std::fmt;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::client::ApiClient;
use crate::authorship::wrapped::WrappedSummary;
use crate::error::AutterError;

const SHARE_ENDPOINT: &str = "/api/cli/wrapped-share";

/// What the server returns for a new or updated share.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WrappedShareResponse {
    pub token: String,
    /// The page: `https://app.autter.dev/wrapped/<token>`.
    pub url: String,
    #[serde(default)]
    pub image_url: Option<String>,
}

/// Why a share or unshare did not happen, each with one line for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareError {
    /// 401: no valid sign-in.
    LoginRequired,
    /// 403: signed in, but not a member of the org that owns the repository.
    NotMember,
    /// 413: the summary is larger than the server accepts.
    TooLarge,
    /// 422: the year has nothing worth a card yet.
    NothingToShare,
    /// 404 on unshare: not one of the user's shares, or already gone.
    NotFound,
    /// 400: the server's own message, e.g. an unsupported schema version.
    Rejected(String),
    /// Transport failures and unexpected statuses.
    Other(String),
}

impl fmt::Display for ShareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShareError::LoginRequired => write!(f, "Not signed in. Run `autter login` first."),
            ShareError::NotMember => write!(
                f,
                "Your account is not a member of the organization that owns this repository."
            ),
            ShareError::TooLarge => write!(f, "The summary is too large to share."),
            ShareError::NothingToShare => write!(
                f,
                "Nothing to share yet: the year needs at least 3 commits with authorship notes."
            ),
            ShareError::NotFound => write!(
                f,
                "No share with that token belongs to you, or it was already removed."
            ),
            ShareError::Rejected(message) => write!(f, "The server rejected the share: {message}"),
            ShareError::Other(message) => write!(f, "Sharing failed: {message}"),
        }
    }
}

impl std::error::Error for ShareError {}

/// The request body: the `--json` document plus the sharing options. The
/// snippet is removed unless prompt text was opted in, whatever the summary
/// holds, so no prompt text can leave the machine by accident.
pub fn build_share_request(
    summary: &WrappedSummary,
    repo_url: Option<&str>,
    include_prompt_text: bool,
    hide_repo: bool,
) -> Value {
    let mut wrapped = serde_json::to_value(summary).unwrap_or(Value::Null);
    if !include_prompt_text
        && let Some(prompt) = wrapped
            .get_mut("longest_prompt")
            .and_then(Value::as_object_mut)
    {
        prompt.insert("snippet".to_string(), Value::Null);
    }
    let mut body = json!({
        "wrapped": wrapped,
        "include_prompt_text": include_prompt_text,
        "hide_repo": hide_repo,
    });
    if let Some(repo_url) = repo_url
        && let Some(object) = body.as_object_mut()
    {
        object.insert("repo_url".to_string(), Value::String(repo_url.to_string()));
    }
    body
}

/// The share token in `--unshare <url-or-token>`: a bare token, or the
/// token at the end of a `/wrapped/<token>` page URL.
pub fn share_token_from(arg: &str) -> Option<String> {
    let trimmed = arg.trim().trim_end_matches('/');
    let token = match trimmed.find("/wrapped/") {
        Some(index) => &trimmed[index + "/wrapped/".len()..],
        None if trimmed.contains("://") || trimmed.contains('/') => return None,
        None => trimmed,
    };
    let token = token
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    let valid = !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    valid.then(|| token.to_string())
}

fn server_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string())
}

fn map_status(status: u16, body: &str, unsharing: bool) -> ShareError {
    match status {
        400 => ShareError::Rejected(server_message(body)),
        401 => ShareError::LoginRequired,
        403 => ShareError::NotMember,
        404 if unsharing => ShareError::NotFound,
        413 => ShareError::TooLarge,
        422 => ShareError::NothingToShare,
        other => {
            let message = server_message(body);
            ShareError::Other(if message.is_empty() {
                format!("status {other}")
            } else {
                format!("status {other}: {message}")
            })
        }
    }
}

impl ApiClient {
    /// Publish (or update) the share page for a summary.
    pub fn share_wrapped(&self, body: &Value) -> Result<WrappedShareResponse, ShareError> {
        let response = self
            .context()
            .post_json(SHARE_ENDPOINT, body)
            .map_err(|e| ShareError::Other(e.to_string()))?;
        let text = response.as_str().unwrap_or("").to_string();
        if !(200..300).contains(&response.status_code) {
            return Err(map_status(response.status_code, &text, false));
        }
        serde_json::from_str(&text)
            .map_err(|e| ShareError::Other(format!("unexpected response: {e}")))
    }

    /// Take a share page down.
    pub fn unshare_wrapped(&self, token: &str) -> Result<(), ShareError> {
        let response = self
            .context()
            .delete(&format!("{SHARE_ENDPOINT}/{token}"))
            .map_err(|e| ShareError::Other(e.to_string()))?;
        if (200..300).contains(&response.status_code) {
            return Ok(());
        }
        let text = response.as_str().unwrap_or("").to_string();
        Err(map_status(response.status_code, &text, true))
    }
}

impl From<ShareError> for AutterError {
    fn from(error: ShareError) -> Self {
        AutterError::Generic(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::ApiContext;
    use crate::authorship::wrapped::{COMMIT_CAP, LongestPrompt, WRAPPED_SCHEMA_VERSION};

    fn summary(snippet: Option<&str>) -> WrappedSummary {
        WrappedSummary {
            schema_version: WRAPPED_SCHEMA_VERSION,
            autter_version: "test",
            year: 2026,
            author: Some("you@company.com".to_string()),
            all_authors: false,
            commits: 10,
            commits_with_authorship: 9,
            capped: false,
            commit_cap: COMMIT_CAP,
            added_lines: 100,
            ai_lines: 60,
            human_lines: 30,
            untracked_lines: 10,
            ai_percent: 60.0,
            by_month: vec![],
            top_agent: None,
            top_model: None,
            agents: vec![],
            models: vec![],
            most_prompted_file: None,
            files: vec![],
            prompts_total: 3,
            prompts_with_transcript: 1,
            longest_prompt: Some(LongestPrompt {
                chars: 40,
                words: 8,
                agent: "claude".to_string(),
                model: "claude-opus-5-5".to_string(),
                commit: "abc".to_string(),
                snippet: snippet.map(str::to_string),
            }),
            busiest_day: None,
            streak_days: 0,
        }
    }

    #[test]
    fn request_carries_the_json_document_and_options() {
        let body = build_share_request(&summary(None), Some("github.com/acme/app"), false, true);
        assert_eq!(body["wrapped"]["schema_version"], "wrapped/1.0.0");
        assert_eq!(body["wrapped"]["year"], 2026);
        assert_eq!(body["repo_url"], "github.com/acme/app");
        assert_eq!(body["include_prompt_text"], false);
        assert_eq!(body["hide_repo"], true);
    }

    #[test]
    fn repo_url_is_omitted_when_unknown() {
        let body = build_share_request(&summary(None), None, false, false);
        assert!(body.get("repo_url").is_none(), "{body}");
    }

    #[test]
    fn snippet_leaves_only_when_opted_in() {
        let body = build_share_request(&summary(Some("secret prompt")), None, false, false);
        assert_eq!(body["wrapped"]["longest_prompt"]["snippet"], Value::Null);
        assert!(!body.to_string().contains("secret prompt"));

        let body = build_share_request(&summary(Some("shared prompt")), None, true, false);
        assert_eq!(
            body["wrapped"]["longest_prompt"]["snippet"],
            "shared prompt"
        );
        assert_eq!(body["include_prompt_text"], true);
    }

    #[test]
    fn tokens_come_from_bare_values_and_page_urls() {
        assert_eq!(share_token_from("abc-123_X"), Some("abc-123_X".to_string()));
        assert_eq!(
            share_token_from("https://app.autter.dev/wrapped/abc-123/"),
            Some("abc-123".to_string())
        );
        assert_eq!(
            share_token_from("https://app.autter.dev/wrapped/abc?utm=x"),
            Some("abc".to_string())
        );
        assert_eq!(share_token_from("https://app.autter.dev/me"), None);
        assert_eq!(share_token_from("https://app.autter.dev/wrapped/"), None);
        assert_eq!(share_token_from("  "), None);
        assert_eq!(share_token_from("a b"), None);
    }

    #[test]
    fn statuses_map_to_one_line_each() {
        assert_eq!(map_status(401, "", false), ShareError::LoginRequired);
        assert_eq!(map_status(403, "", false), ShareError::NotMember);
        assert_eq!(map_status(413, "", false), ShareError::TooLarge);
        assert_eq!(map_status(422, "", false), ShareError::NothingToShare);
        assert_eq!(map_status(404, "", true), ShareError::NotFound);
        assert_eq!(
            map_status(400, r#"{"error":"unsupported schema_version"}"#, false),
            ShareError::Rejected("unsupported schema_version".to_string())
        );
        assert_eq!(
            map_status(500, "boom", false),
            ShareError::Other("status 500: boom".to_string())
        );
        assert!(
            ShareError::LoginRequired
                .to_string()
                .contains("autter login")
        );
    }

    fn client(server: &mockito::Server) -> ApiClient {
        ApiClient::new(ApiContext::with_auth(
            Some(server.url()),
            "test-access-token".to_string(),
        ))
    }

    #[test]
    fn share_posts_the_body_with_a_bearer_token() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/cli/wrapped-share")
            .match_header("authorization", "Bearer test-access-token")
            .match_header("content-type", "application/json")
            .match_body(mockito::Matcher::PartialJson(json!({
                "hide_repo": true,
                "include_prompt_text": false,
                "wrapped": {"year": 2026}
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"token":"tok1","url":"https://app.autter.dev/wrapped/tok1","image_url":"https://app.autter.dev/public/wrapped/tok1/card.png"}"#,
            )
            .create();

        let body = build_share_request(&summary(None), None, false, true);
        let response = client(&server).share_wrapped(&body).unwrap();
        mock.assert();
        assert_eq!(
            response,
            WrappedShareResponse {
                token: "tok1".to_string(),
                url: "https://app.autter.dev/wrapped/tok1".to_string(),
                image_url: Some("https://app.autter.dev/public/wrapped/tok1/card.png".to_string()),
            }
        );
    }

    #[test]
    fn share_errors_are_mapped() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/cli/wrapped-share")
            .with_status(400)
            .with_body(r#"{"error":"unsupported schema_version wrapped/9.0.0"}"#)
            .create();
        let body = build_share_request(&summary(None), None, false, false);
        let error = client(&server).share_wrapped(&body).unwrap_err();
        mock.assert();
        assert_eq!(
            error,
            ShareError::Rejected("unsupported schema_version wrapped/9.0.0".to_string())
        );
    }

    #[test]
    fn unshare_deletes_by_token() {
        let mut server = mockito::Server::new();
        let ok = server
            .mock("DELETE", "/api/cli/wrapped-share/tok1")
            .match_header("authorization", "Bearer test-access-token")
            .with_status(204)
            .create();
        let gone = server
            .mock("DELETE", "/api/cli/wrapped-share/tok2")
            .with_status(404)
            .with_body(r#"{"error":"not found"}"#)
            .create();

        assert_eq!(client(&server).unshare_wrapped("tok1"), Ok(()));
        assert_eq!(
            client(&server).unshare_wrapped("tok2"),
            Err(ShareError::NotFound)
        );
        ok.assert();
        gone.assert();
    }
}
