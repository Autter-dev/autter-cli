use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::StatSnapshot;
use crate::commands::checkpoint_agent::orchestrator::CheckpointRequest;
use crate::daemon::domain::RepoContext;
use crate::metrics::MetricEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// What this service understands beyond the original control protocol. It
/// answers `ping` with `{"protocol": N}`; a service that answers `ping`
/// without it, or with an error because it does not know `ping`, is
/// version 0.
///
/// * 0: requests are answered once handled. Nothing is sent for a checkpoint
///   until it has been processed.
/// * 1: a `checkpoint.run` with `"receipt": true` is acknowledged with a
///   receipt ([`ControlResponse::receipt`]) as soon as the service has read
///   it, before processing it. The final answer follows on the same
///   connection.
pub const CONTROL_PROTOCOL_VERSION: u32 = 1;

/// The first protocol version that sends checkpoint receipts.
pub const CHECKPOINT_RECEIPT_PROTOCOL: u32 = 1;

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum ControlRequest {
    #[serde(rename = "checkpoint.run")]
    CheckpointRun {
        request: Box<CheckpointRequest>,
        /// Ask for a receipt as soon as the request has been read (protocol
        /// 1). A service of protocol 0 ignores the field. Left out when
        /// false, so the request is byte-for-byte what older clients sent.
        #[serde(default, skip_serializing_if = "is_false")]
        receipt: bool,
    },
    #[serde(rename = "status.family")]
    StatusFamily { repo_working_dir: String },
    #[serde(rename = "telemetry.submit")]
    SubmitTelemetry { envelopes: Vec<TelemetryEnvelope> },
    #[serde(rename = "cas.submit")]
    SubmitCas { records: Vec<CasSyncPayload> },
    /// Signal the daemon that new notes are pending in notes-db and should be flushed.
    #[serde(rename = "notes.flush")]
    FlushNotes,
    #[serde(rename = "wrapper.pre_state")]
    WrapperPreState {
        invocation_id: String,
        repo_working_dir: String,
        repo_context: RepoContext,
    },
    #[serde(rename = "wrapper.post_state")]
    WrapperPostState {
        invocation_id: String,
        repo_working_dir: String,
        repo_context: RepoContext,
    },
    #[serde(rename = "snapshot.watermarks")]
    SnapshotWatermarks { repo_working_dir: String },
    #[serde(rename = "bash_session.start")]
    BashSessionStart {
        repo_work_dir: String,
        session_id: String,
        tool_use_id: String,
        agent_id: AgentId,
        metadata: HashMap<String, String>,
        stat_snapshot: Box<StatSnapshot>,
    },
    #[serde(rename = "bash_session.end")]
    BashSessionEnd {
        session_id: String,
        tool_use_id: String,
    },
    #[serde(rename = "bash_session.query")]
    BashSessionQuery { repo_work_dir: String },
    #[serde(rename = "bash_snapshot.query")]
    BashSnapshotQuery {
        session_id: String,
        tool_use_id: String,
    },
    #[serde(rename = "shutdown")]
    Shutdown,
    /// Does nothing and answers `ok`, with `{"protocol": N}`
    /// ([`CONTROL_PROTOCOL_VERSION`]) in `data`. A reply proves the service is
    /// handling requests, not just accepting connections. A service too old
    /// to know this request still answers (with an error), which proves the
    /// same.
    #[serde(rename = "ping")]
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    pub fn ok(seq: Option<u64>, data: Option<Value>) -> Self {
        Self {
            ok: true,
            seq,
            data,
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            seq: None,
            data: None,
            error: Some(msg.into()),
        }
    }

    /// The service did not take the request at all (too busy, or shutting
    /// down) and is closing the connection. Marked in `data` so a client can
    /// tell this from a request that was handled and failed.
    pub fn refused(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            seq: None,
            data: Some(serde_json::json!({ "refused": true })),
            error: Some(msg.into()),
        }
    }

    /// The service has read a checkpoint and taken it on; it has not
    /// processed it yet. Sent before the final answer, only when the request
    /// asked for it.
    pub fn receipt() -> Self {
        Self::ok(None, Some(serde_json::json!({ "receipt": true })))
    }

    pub fn is_receipt(&self) -> bool {
        self.ok
            && self
                .data
                .as_ref()
                .and_then(|data| data.get("receipt"))
                .and_then(Value::as_bool)
                == Some(true)
    }

    /// The answer to `ping`.
    pub fn pong() -> Self {
        Self::ok(
            None,
            Some(serde_json::json!({ "protocol": CONTROL_PROTOCOL_VERSION })),
        )
    }

    /// The protocol version a service reported in its answer to `ping`: 0 for
    /// a service that predates protocol versions (it answers without one, or
    /// with an error because it does not know `ping`). `None` when the answer
    /// says nothing about the service's version: it refused the connection.
    pub fn ping_protocol(&self) -> Option<u32> {
        if self.is_refusal() {
            return None;
        }
        let reported = self
            .data
            .as_ref()
            .and_then(|data| data.get("protocol"))
            .and_then(Value::as_u64)
            .filter(|_| self.ok);
        Some(reported.map_or(0, |version| u32::try_from(version).unwrap_or(u32::MAX)))
    }

    pub fn is_refusal(&self) -> bool {
        !self.ok
            && self
                .data
                .as_ref()
                .and_then(|data| data.get("refused"))
                .and_then(Value::as_bool)
                == Some(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashSessionQueryResponse {
    pub active: bool,
    pub agent_id: Option<AgentId>,
    pub session_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub metadata: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashSnapshotQueryResponse {
    pub found: bool,
    pub stat_snapshot: Option<StatSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FamilyStatus {
    pub family_key: String,
    pub latest_seq: u64,
    pub last_error: Option<String>,
    /// Side effects that are still running for this family. Older daemons do
    /// not send this field, so it reads as 0.
    #[serde(default)]
    pub inflight_effects: usize,
    /// Git commands and checkpoints received for this family but not yet
    /// applied. Older daemons do not send this field, so it reads as 0.
    #[serde(default)]
    pub queued_entries: usize,
    /// Of `queued_entries`, how many are checkpoints.
    #[serde(default)]
    pub queued_checkpoints: usize,
}

impl FamilyStatus {
    /// Work the service has accepted for this repository and not finished.
    pub fn pending_work(&self) -> usize {
        self.inflight_effects.saturating_add(self.queued_entries)
    }
}

/// A telemetry envelope sent from client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TelemetryEnvelope {
    Error {
        timestamp: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<Value>,
    },
    Performance {
        timestamp: String,
        operation: String,
        duration_ms: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tags: Option<std::collections::HashMap<String, String>>,
    },
    Message {
        timestamp: String,
        message: String,
        level: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<Value>,
    },
    Metrics {
        events: Vec<MetricEvent>,
    },
}

/// A CAS object payload sent from client to daemon for background upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasSyncPayload {
    pub hash: String,
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `checkpoint.run` as a service of protocol 0 parses it: no `receipt`.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "method", content = "params")]
    enum ProtocolZeroRequest {
        #[serde(rename = "checkpoint.run")]
        CheckpointRun { request: Value },
    }

    fn checkpoint_json(receipt: bool) -> String {
        let request: CheckpointRequest = serde_json::from_value(serde_json::json!({
            "trace_id": "t",
            "checkpoint_kind": "Human",
            "agent_id": null,
            "files": [],
            "path_role": "will_edit",
            "stream_source": null,
            "metadata": {}
        }))
        .expect("sample checkpoint request");
        serde_json::to_string(&ControlRequest::CheckpointRun {
            request: Box::new(request),
            receipt,
        })
        .unwrap()
    }

    #[test]
    fn receipt_request_is_understood_by_an_older_service() {
        let with_receipt = checkpoint_json(true);
        assert!(with_receipt.contains("\"receipt\":true"), "{with_receipt}");
        let ProtocolZeroRequest::CheckpointRun { request } =
            serde_json::from_str::<ProtocolZeroRequest>(&with_receipt)
                .expect("a protocol-0 service must still parse a checkpoint asking for a receipt");
        assert_eq!(request["trace_id"], "t");
    }

    #[test]
    fn checkpoint_without_receipt_is_sent_as_before() {
        let plain = checkpoint_json(false);
        assert!(!plain.contains("receipt"), "{plain}");
        // And a request from an older client reads as not wanting one.
        match serde_json::from_str::<ControlRequest>(&plain).unwrap() {
            ControlRequest::CheckpointRun { receipt, .. } => assert!(!receipt),
            other => panic!("unexpected {other:?}"),
        }
        match serde_json::from_str::<ControlRequest>(&checkpoint_json(true)).unwrap() {
            ControlRequest::CheckpointRun { receipt, .. } => assert!(receipt),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn receipt_is_an_ok_answer_and_nothing_else_is_a_receipt() {
        let receipt = ControlResponse::receipt();
        assert!(receipt.ok && receipt.is_receipt() && !receipt.is_refusal());
        assert!(!ControlResponse::ok(None, None).is_receipt());
        assert!(!ControlResponse::refused("busy").is_receipt());
        assert!(!ControlResponse::err("no").is_receipt());
    }

    #[test]
    fn ping_protocol_tells_older_services_from_refusals() {
        assert_eq!(
            ControlResponse::pong().ping_protocol(),
            Some(CONTROL_PROTOCOL_VERSION)
        );
        // This branch's first `ping`: ok, no version.
        assert_eq!(ControlResponse::ok(None, None).ping_protocol(), Some(0));
        // A released service that does not know `ping` at all.
        assert_eq!(
            ControlResponse::err("invalid control request: unknown variant `ping`").ping_protocol(),
            Some(0)
        );
        // A refusal says nothing about the version.
        assert_eq!(ControlResponse::refused("busy").ping_protocol(), None);
    }
}
