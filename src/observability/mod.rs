use std::collections::HashMap;
use std::time::Duration;

use crate::metrics::MetricEvent;

pub mod performance_targets;

/// Maximum events per metrics envelope
pub const MAX_METRICS_PER_ENVELOPE: usize = 1000;

/// Submit telemetry envelopes via the best available path:
/// 1. External daemon control socket (wrapper processes)
/// 2. In-process daemon telemetry worker (daemon process itself)
/// 3. Silently drop if neither is available
fn submit_telemetry_envelope(envelopes: Vec<crate::daemon::TelemetryEnvelope>) {
    if crate::daemon::telemetry_handle::daemon_telemetry_available() {
        crate::daemon::telemetry_handle::submit_telemetry(envelopes);
    } else if crate::daemon::daemon_process_active() {
        crate::daemon::telemetry_worker::submit_daemon_internal_telemetry(envelopes);
    }
}

/// Report a tracked CLI error to *both* telemetry backends:
///
/// 1. **PostHog Error Tracking** (when `report_to_posthog` is set): emitted as a
///    `TelemetryEnvelope::Error`, which the daemon forwards as a `$exception`.
/// 2. **The org's own database**: emitted as a `CliError` metric event, which
///    flows through the metrics rail into the generic `cli_metrics` table (and
///    falls back to the local SQLite queue when offline / logged out).
///
/// Both paths are consent-gated and best-effort -- nothing here can fail the
/// caller. `report_to_posthog` exists so callers that already surface the error
/// to PostHog by another route (e.g. the panic hook, which always emits the
/// `$exception`) can record *only* the org-DB metric and avoid double-counting.
///
/// `kind` is a stable, low-cardinality category (e.g.
/// `"git_proxy_panic_recovery"`, `"checkpoint_usage_error"`). `command` is the
/// autter/git subcommand involved, when known.
pub fn report_cli_error(
    kind: &str,
    message: &str,
    command: Option<&str>,
    context: Option<&str>,
    report_to_posthog: bool,
) {
    // 1. Org database, via the metrics rail.
    let mut values = crate::metrics::CliErrorValues::new()
        .kind(kind)
        .message(message);
    if let Some(command) = command {
        values = values.command(command);
    }
    if let Some(context) = context {
        values = values.context(context);
    }
    let attrs = crate::metrics::EventAttributes::with_version(env!("CARGO_PKG_VERSION"));
    crate::metrics::record(values, attrs);

    // 2. PostHog Error Tracking, via the error rail.
    if report_to_posthog {
        let envelope = crate::daemon::TelemetryEnvelope::Error {
            timestamp: chrono::Utc::now().to_rfc3339(),
            message: format!("{kind}: {message}"),
            context: Some(serde_json::json!({
                "kind": kind,
                "command": command,
                "context": context,
            })),
        };
        submit_telemetry_envelope(vec![envelope]);
    }
}

/// Log an error to Sentry (via daemon telemetry worker)
pub fn log_error(error: &dyn std::error::Error, context: Option<serde_json::Value>) {
    let envelope = crate::daemon::TelemetryEnvelope::Error {
        timestamp: chrono::Utc::now().to_rfc3339(),
        message: error.to_string(),
        context,
    };
    submit_telemetry_envelope(vec![envelope]);
}

/// Install a panic hook that reports unexpected panics as error events
/// (surfacing in PostHog Error Tracking via the daemon) while preserving the
/// default behavior of printing the panic to stderr.
///
/// Reporting is best-effort and routes through the same consent-gated path as
/// every other event: the daemon only forwards to PostHog when the user has
/// opted into telemetry.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<dyn Any>".to_string()
        };

        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));

        let envelope = crate::daemon::TelemetryEnvelope::Error {
            timestamp: chrono::Utc::now().to_rfc3339(),
            message: format!("panic: {payload}"),
            context: Some(serde_json::json!({
                "kind": "panic",
                "location": location,
            })),
        };
        submit_telemetry_envelope(vec![envelope]);

        // Preserve normal panic output (and any abort behavior).
        default_hook(info);
    }));
}

/// Log a performance metric to Sentry (via daemon telemetry worker)
pub fn log_performance(
    operation: &str,
    duration: Duration,
    context: Option<serde_json::Value>,
    tags: Option<HashMap<String, String>>,
) {
    let envelope = crate::daemon::TelemetryEnvelope::Performance {
        timestamp: chrono::Utc::now().to_rfc3339(),
        operation: operation.to_string(),
        duration_ms: duration.as_millis(),
        context,
        tags,
    };
    submit_telemetry_envelope(vec![envelope]);
}

/// Log a message to Sentry (info, warning, etc.) (via daemon telemetry worker)
#[allow(dead_code)]
pub fn log_message(message: &str, level: &str, context: Option<serde_json::Value>) {
    let envelope = crate::daemon::TelemetryEnvelope::Message {
        timestamp: chrono::Utc::now().to_rfc3339(),
        message: message.to_string(),
        level: level.to_string(),
        context,
    };
    submit_telemetry_envelope(vec![envelope]);
}

/// Log a batch of metric events (via daemon telemetry worker).
///
/// Events are batched into envelopes of up to 1000 events each.
pub fn log_metrics(
    #[cfg_attr(any(test, feature = "test-support"), allow(unused))] events: Vec<MetricEvent>,
) {
    #[cfg(any(test, feature = "test-support"))]
    return;

    #[cfg(not(any(test, feature = "test-support")))]
    {
        if events.is_empty() {
            return;
        }

        // Split into chunks of MAX_METRICS_PER_ENVELOPE
        for chunk in events.chunks(MAX_METRICS_PER_ENVELOPE) {
            let envelope = crate::daemon::TelemetryEnvelope::Metrics {
                events: chunk.to_vec(),
            };
            submit_telemetry_envelope(vec![envelope]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    // Test error logging
    #[test]
    fn test_log_error_no_panic() {
        use std::io;
        let error = io::Error::new(io::ErrorKind::NotFound, "test error");
        log_error(&error, None);
    }

    #[test]
    fn test_log_error_with_context() {
        use serde_json::json;
        use std::io;
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "access denied");
        let context = json!({"file": "test.txt", "operation": "read"});
        log_error(&error, Some(context));
    }

    // Test performance logging
    #[test]
    fn test_log_performance_basic() {
        log_performance("test_operation", Duration::from_millis(100), None, None);
    }

    #[test]
    fn test_log_performance_with_context() {
        use serde_json::json;
        let context = json!({"files": 5, "lines": 100});
        log_performance("test_op", Duration::from_secs(1), Some(context), None);
    }

    #[test]
    fn test_log_performance_with_tags() {
        let mut tags = HashMap::new();
        tags.insert("command".to_string(), "commit".to_string());
        tags.insert("repo".to_string(), "test".to_string());
        log_performance("commit_op", Duration::from_millis(500), None, Some(tags));
    }

    // Test message logging
    #[test]
    fn test_log_message_basic() {
        log_message("test message", "info", None);
    }

    #[test]
    fn test_log_message_with_context() {
        use serde_json::json;
        let context = json!({"user": "test", "action": "login"});
        log_message("user logged in", "info", Some(context));
    }

    #[test]
    fn test_log_message_warning() {
        log_message("warning message", "warning", None);
    }

    // Test metrics logging
    #[test]
    fn test_log_metrics_empty() {
        log_metrics(vec![]);
    }

    // Test constants
    #[test]
    fn test_max_metrics_per_envelope() {
        assert_eq!(MAX_METRICS_PER_ENVELOPE, 1000);
    }
}
