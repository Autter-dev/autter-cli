//! Custom tracing Layer that forwards ERROR-level events to Sentry
//! via the existing daemon telemetry worker pipeline.

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// A tracing Layer that intercepts ERROR-level events and routes them
/// to the daemon's telemetry worker as `TelemetryEnvelope::Error` events,
/// which get forwarded to both enterprise and OSS Sentry DSNs.
pub struct SentryLayer;

struct MessageVisitor {
    message: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl MessageVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            fields: serde_json::Map::new(),
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(format!("{:?}", value)),
            );
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }
}

/// Combine the static tracing message with a structured `error` field (as
/// emitted by `%error`) so the resulting exception value reflects the real
/// underlying cause. Returns the message unchanged when there is no non-empty
/// `error` field to promote.
fn message_with_promoted_error(
    message: &str,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> String {
    match fields.get("error").and_then(|v| v.as_str()) {
        Some(error) if !error.is_empty() && !message.is_empty() => {
            format!("{}: {}", message, error)
        }
        Some(error) if !error.is_empty() => error.to_string(),
        _ => message.to_string(),
    }
}

impl<S: Subscriber> Layer<S> for SentryLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() != Level::ERROR {
            return;
        }

        let mut visitor = MessageVisitor::new();
        event.record(&mut visitor);

        // Promote a structured `error` field into the message. Downstream this
        // becomes the exception value/`$exception_list` entry, so distinct
        // underlying causes fingerprint separately in error tracking instead of
        // every event collapsing onto the static log message string (e.g. all
        // side-effect failures grouping under "command side effect failed").
        let message = message_with_promoted_error(&visitor.message, &visitor.fields);

        let context = if visitor.fields.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(visitor.fields))
        };

        let envelope = crate::daemon::control_api::TelemetryEnvelope::Error {
            timestamp: chrono::Utc::now().to_rfc3339(),
            message,
            context,
        };

        crate::daemon::telemetry_worker::submit_daemon_internal_telemetry(vec![envelope]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn promotes_error_field_into_message() {
        let f = fields(&[(
            "error",
            json!(
                "Git CLI (git ls-tree HEAD) failed with exit code 128: fatal: cannot change to '/tmp/.tmpXXXX': No such file or directory"
            ),
        )]);
        let message = message_with_promoted_error("command side effect failed", &f);
        assert_eq!(
            message,
            "command side effect failed: Git CLI (git ls-tree HEAD) failed with exit code 128: fatal: cannot change to '/tmp/.tmpXXXX': No such file or directory"
        );
    }

    #[test]
    fn distinct_errors_produce_distinct_messages() {
        let a = fields(&[("error", json!("cause A"))]);
        let b = fields(&[("error", json!("cause B"))]);
        let base = "command side effect failed";
        assert_ne!(
            message_with_promoted_error(base, &a),
            message_with_promoted_error(base, &b),
        );
    }

    #[test]
    fn leaves_message_unchanged_without_error_field() {
        let f = fields(&[("family", json!("abc")), ("seq", json!(3))]);
        assert_eq!(
            message_with_promoted_error("something happened", &f),
            "something happened"
        );
    }

    #[test]
    fn empty_error_field_is_ignored() {
        let f = fields(&[("error", json!(""))]);
        assert_eq!(
            message_with_promoted_error("something happened", &f),
            "something happened"
        );
    }

    #[test]
    fn uses_error_as_message_when_base_is_empty() {
        let f = fields(&[("error", json!("standalone cause"))]);
        assert_eq!(message_with_promoted_error("", &f), "standalone cause");
    }
}
