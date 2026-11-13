//! Session usage snapshots for agents that keep token usage outside the events
//! autter streams.
//!
//! autter forwards agent transcript events verbatim, so for most agents the
//! token usage (including prompt-cache reads and writes) travels inside the raw
//! event and the backend reads it from the agent's own shape. A few agents keep
//! usage somewhere else:
//!
//! - Droid writes a running `tokenUsage` total into `<session>.settings.json`,
//!   next to the transcript, and never into the transcript itself.
//! - Continue CLI writes a running `usage` total at the top level of the session
//!   file, while autter streams only the `history` items.
//!
//! For those, the reader appends one synthetic event per batch that read new
//! records, built here. The counts follow one convention, documented in
//! `docs/agent-usage.md`:
//!
//! - the counts are disjoint: `input_tokens` excludes `cache_read_tokens` and
//!   `cache_write_tokens` (`input_includes_cache` is always `false`);
//! - `output_tokens` includes reasoning; `reasoning_tokens` is a subset of it;
//! - `scope` is `session_total`: every count is the session's running total
//!   when the snapshot was taken, so the backend must diff snapshots rather
//!   than sum them;
//! - `cost_usd` is the cost the agent itself reported, when it reports one.

use serde_json::{Value, json};

/// `type` of the synthetic event. Never produced by an agent.
pub const USAGE_SNAPSHOT_EVENT_TYPE: &str = "autter_usage_snapshot";

/// Version of the snapshot payload. Bump only for breaking changes.
pub const USAGE_SNAPSHOT_VERSION: u64 = 1;

/// A session's running token totals in autter's disjoint convention.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionUsageSnapshot {
    /// Where the totals came from, e.g. `droid_settings`.
    pub source: &'static str,
    pub model: Option<String>,
    /// Input tokens that were neither read from nor written to a prompt cache.
    pub input_tokens: u64,
    /// Output tokens, reasoning included.
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Reasoning tokens, a subset of `output_tokens`, when reported.
    pub reasoning_tokens: Option<u64>,
    /// Cost the agent itself reported, in USD.
    pub cost_usd: Option<f64>,
}

impl SessionUsageSnapshot {
    fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
            && self.cost_usd.unwrap_or(0.0) <= 0.0
    }

    /// The synthetic event, or `None` when the snapshot carries no usage.
    pub fn to_event(&self, timestamp: Option<String>) -> Option<Value> {
        if self.is_empty() {
            return None;
        }
        let mut event = json!({
            "type": USAGE_SNAPSHOT_EVENT_TYPE,
            "autter_usage_snapshot": {
                "version": USAGE_SNAPSHOT_VERSION,
                "source": self.source,
                "scope": "session_total",
                "model": self.model,
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
                "cache_read_tokens": self.cache_read_tokens,
                "cache_write_tokens": self.cache_write_tokens,
                "reasoning_tokens": self.reasoning_tokens,
                "cost_usd": self.cost_usd,
                "input_includes_cache": false,
            },
        });
        if let Some(ts) = timestamp {
            event["timestamp"] = Value::String(ts);
        }
        Some(event)
    }
}

/// A non-negative integer count, accepting integers, floats and numeric strings.
pub fn count(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f > 0.0).map(|f| f as u64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

fn optional_count(value: Option<&Value>) -> Option<u64> {
    value.filter(|v| !v.is_null()).map(|v| count(Some(v)))
}

/// Droid's `<session>.settings.json`:
/// `{"model": "...", "tokenUsage": {"inputTokens", "outputTokens",
/// "cacheCreationTokens", "cacheReadTokens", "thinkingTokens"}}`.
///
/// Droid's `inputTokens` is already disjoint from the cache counts (observed:
/// `inputTokens` 6,743 next to `cacheReadTokens` 42,496). `thinkingTokens` is
/// kept as a subset of output, as both Anthropic and OpenAI bill it as output.
pub fn from_droid_settings(settings: &Value) -> Option<SessionUsageSnapshot> {
    let usage = settings.get("tokenUsage")?.as_object()?;
    Some(SessionUsageSnapshot {
        source: "droid_settings",
        model: settings
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from),
        input_tokens: count(usage.get("inputTokens")),
        output_tokens: count(usage.get("outputTokens")),
        cache_read_tokens: count(usage.get("cacheReadTokens")),
        cache_write_tokens: count(usage.get("cacheCreationTokens")),
        reasoning_tokens: optional_count(usage.get("thinkingTokens")),
        cost_usd: None,
    })
}

/// Continue CLI's session-level `usage`:
/// `{"totalCost", "promptTokens", "completionTokens",
/// "promptTokensDetails": {"cachedTokens", "cacheWriteTokens"}}`.
///
/// Continue sums each call's `prompt_tokens` and its adapter's
/// `cache_read_tokens` / `cache_write_tokens`. Its Anthropic adapter sets
/// `prompt_tokens` to Anthropic's `input_tokens`, which already excludes cache
/// reads and writes, and only that adapter fills the cache fields, so the
/// totals are taken as disjoint.
pub fn from_continue_session_usage(usage: &Value) -> Option<SessionUsageSnapshot> {
    let usage = usage.as_object()?;
    let details = usage.get("promptTokensDetails");
    Some(SessionUsageSnapshot {
        source: "continue_session",
        model: None,
        input_tokens: count(usage.get("promptTokens")),
        output_tokens: count(usage.get("completionTokens")),
        cache_read_tokens: count(details.and_then(|d| d.get("cachedTokens"))),
        cache_write_tokens: count(details.and_then(|d| d.get("cacheWriteTokens"))),
        reasoning_tokens: None,
        cost_usd: usage
            .get("totalCost")
            .and_then(Value::as_f64)
            .filter(|c| c.is_finite() && *c > 0.0),
    })
}

/// File modification time as an RFC 3339 timestamp, for a snapshot's `timestamp`.
pub fn file_mtime_rfc3339(path: &std::path::Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let dt: chrono::DateTime<chrono::Utc> = modified.into();
    Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn droid_settings_fixture_maps_cache_fields() {
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/droid-session.settings.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let snap = from_droid_settings(&settings).unwrap();
        assert_eq!(snap.source, "droid_settings");
        assert_eq!(snap.model.as_deref(), Some("custom:BYOK-GPT-5-MINI-0"));
        assert_eq!(snap.input_tokens, 6743);
        assert_eq!(snap.output_tokens, 1450);
        assert_eq!(snap.cache_read_tokens, 42496);
        assert_eq!(snap.cache_write_tokens, 0);
        assert_eq!(snap.reasoning_tokens, Some(1088));
        assert_eq!(snap.cost_usd, None);

        let event = snap
            .to_event(Some("2026-01-28T16:57:30.000Z".into()))
            .unwrap();
        assert_eq!(event["type"], USAGE_SNAPSHOT_EVENT_TYPE);
        assert_eq!(event["timestamp"], "2026-01-28T16:57:30.000Z");
        let s = &event["autter_usage_snapshot"];
        assert_eq!(s["version"], 1);
        assert_eq!(s["scope"], "session_total");
        assert_eq!(s["input_includes_cache"], false);
        assert_eq!(s["cache_read_tokens"], 42496);
        assert_eq!(s["cache_write_tokens"], 0);
        assert_eq!(s["reasoning_tokens"], 1088);
        assert!(s["cost_usd"].is_null());
    }

    #[test]
    fn droid_settings_without_token_usage_is_none() {
        let settings = json!({"model": "claude-opus-4-6"});
        assert!(from_droid_settings(&settings).is_none());
    }

    #[test]
    fn continue_session_usage_fixture_maps_cache_fields_and_cost() {
        let session: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/continue-cli-session-simple.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let snap = from_continue_session_usage(&session["usage"]).unwrap();
        assert_eq!(snap.source, "continue_session");
        assert_eq!(snap.input_tokens, 1813);
        assert_eq!(snap.output_tokens, 305);
        assert_eq!(snap.cache_read_tokens, 4366);
        assert_eq!(snap.cache_write_tokens, 2183);
        assert_eq!(snap.cost_usd, Some(0.002423));

        let event = snap.to_event(None).unwrap();
        assert!(event.get("timestamp").is_none());
        assert_eq!(event["autter_usage_snapshot"]["cost_usd"], 0.002423);
        assert!(event["autter_usage_snapshot"]["model"].is_null());
    }

    #[test]
    fn empty_usage_produces_no_event() {
        let snap = from_continue_session_usage(&json!({
            "totalCost": 0,
            "promptTokens": 0,
            "completionTokens": 0,
        }))
        .unwrap();
        assert!(snap.to_event(None).is_none());
    }

    #[test]
    fn counts_accept_strings_and_floats_and_reject_negatives() {
        assert_eq!(count(Some(&json!("42"))), 42);
        assert_eq!(count(Some(&json!(42.0))), 42);
        assert_eq!(count(Some(&json!(-3))), 0);
        assert_eq!(count(Some(&json!(null))), 0);
        assert_eq!(count(None), 0);
    }
}
