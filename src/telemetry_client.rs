//! Shared PostHog client + local "what we send" mirror.
//!
//! This module is the single place that knows how to talk to PostHog and what
//! data leaves the machine. Two callers use it:
//!
//! - The daemon telemetry worker (`daemon::telemetry_worker`) — forwards usage
//!   messages and errors when the user has consented to telemetry.
//! - First-run onboarding (`commands::onboard`) — fires a one-off install event
//!   right after the user opts in.
//!
//! ## Privacy model
//!
//! Telemetry is gated entirely on the user's onboarding choice
//! (`telemetry_oss` in `~/.autter/config.json`, surfaced as
//! [`Config::is_telemetry_oss_disabled`]). When enabled, every payload is:
//!
//! 1. Restricted to **anonymous, non-personal** data — a random install UUID
//!    (`distinct_id`), coarse device facts (OS, CPU architecture, core count),
//!    and the autter version. No hostname, username, file paths, repo URLs, IP,
//!    or prompt content is ever added here.
//! 2. **Mirrored verbatim** to a local append-only log at
//!    `~/.autter/internal/telemetry.log` *before* it is sent, so the user can
//!    audit exactly what was transmitted (see [`local_log_path`]).
//!
//! If the user declines telemetry, [`PostHogClient::resolve`] returns `None`
//! and nothing is logged or sent.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::config::{Config, internal_dir_path};

/// Default PostHog ingestion host (US cloud). Overridable via `POSTHOG_HOST`.
const DEFAULT_POSTHOG_HOST: &str = "https://us.i.posthog.com";

/// Cap the local mirror log so it can never grow without bound. When the file
/// exceeds this size it is truncated and started fresh.
const LOCAL_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB

/// Path to the human-auditable telemetry mirror log
/// (`~/.autter/internal/telemetry.log`).
///
/// Every event sent to PostHog is appended here as one JSON object per line so
/// users can inspect precisely what left their machine.
pub fn local_log_path() -> Option<PathBuf> {
    internal_dir_path().map(|dir| dir.join("telemetry.log"))
}

/// Coarse, non-identifying device facts attached to every event.
///
/// Intentionally limited to values that cannot single out a user: OS family,
/// CPU architecture, logical core count, and the autter version. No hostname,
/// username, locale, IP, MAC, or serial number is collected.
pub fn safe_device_properties() -> BTreeMap<String, Value> {
    let mut props = BTreeMap::new();
    props.insert("os".to_string(), json!(std::env::consts::OS));
    props.insert("os_family".to_string(), json!(std::env::consts::FAMILY));
    props.insert("arch".to_string(), json!(std::env::consts::ARCH));
    props.insert("version".to_string(), json!(env!("CARGO_PKG_VERSION")));

    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    props.insert("cpu_count".to_string(), json!(cpu_count));

    props
}

/// A resolved PostHog endpoint the caller is permitted to send to.
///
/// Construct via [`PostHogClient::resolve`], which returns `None` whenever
/// telemetry is disabled or no API key is configured — making it impossible to
/// send without consent.
pub struct PostHogClient {
    api_key: String,
    host: String,
}

impl PostHogClient {
    /// Resolve a client if (and only if) telemetry is enabled and an API key is
    /// available. Returns `None` when the user has opted out of telemetry, so
    /// callers cannot accidentally bypass consent.
    pub fn resolve(config: &Config) -> Option<Self> {
        if config.is_telemetry_oss_disabled() {
            return None;
        }
        Self::resolve_unchecked()
    }

    /// Resolve a client without consulting the global telemetry toggle.
    ///
    /// Used only by onboarding, where the user has *just* granted consent in the
    /// same flow and the in-memory [`Config`] singleton predates that choice.
    /// All other callers must use [`PostHogClient::resolve`].
    pub fn resolve_unchecked() -> Option<Self> {
        let api_key = std::env::var("POSTHOG_API_KEY")
            .ok()
            .or_else(|| option_env!("POSTHOG_API_KEY").map(|s| s.to_string()))
            .filter(|s| !s.is_empty())?;

        let host = std::env::var("POSTHOG_HOST")
            .ok()
            .or_else(|| option_env!("POSTHOG_HOST").map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_POSTHOG_HOST.to_string());

        Some(Self { api_key, host })
    }

    /// Capture an event: merge in safe device properties, mirror the payload to
    /// the local audit log, then POST it to PostHog. Best-effort — failures are
    /// swallowed so telemetry never disrupts the user's git workflow.
    pub fn capture(
        &self,
        distinct_id: &str,
        event: &str,
        extra_properties: BTreeMap<String, Value>,
    ) {
        let mut properties = safe_device_properties();
        properties.extend(extra_properties);

        // Mirror locally *before* sending so the on-disk log is a faithful
        // record of what we transmit.
        append_local_log(distinct_id, event, &properties);

        let endpoint = format!("{}/capture/", self.host.trim_end_matches('/'));
        let payload = json!({
            "api_key": self.api_key,
            "event": event,
            "distinct_id": distinct_id,
            "properties": properties,
        });

        let agent = crate::http::build_agent(Some(30));
        let request = agent
            .post(&endpoint)
            .set("Content-Type", "application/json");
        let _ = crate::http::send_with_body(
            request,
            &serde_json::to_string(&payload).unwrap_or_default(),
        );
    }
}

/// Append one event to the local audit log (`telemetry.log`), best-effort.
fn append_local_log(distinct_id: &str, event: &str, properties: &BTreeMap<String, Value>) {
    let Some(path) = local_log_path() else {
        return;
    };

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Reset the file if it has grown past the cap so it can't bloat unbounded.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOCAL_LOG_MAX_BYTES) {
        let _ = std::fs::remove_file(&path);
    }

    let line = json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "event": event,
        "distinct_id": distinct_id,
        "properties": properties,
    });

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        && let Ok(serialized) = serde_json::to_string(&line)
    {
        let _ = writeln!(file, "{serialized}");
    }
}
