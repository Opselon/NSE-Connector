//! NSE JSON contract envelopes (spec §6).
//!
//! Every message the Connector sends carries protocol/schema versions,
//! correlation ids, identity resolved by the server, source metadata, and
//! both raw + normalized data where appropriate. Broker timestamps are
//! preserved verbatim inside payloads.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Connector ↔ NSE Core protocol version.
pub const PROTOCOL_VERSION: &str = "1.0";
/// Message schema version (bump on contract changes).
pub const SCHEMA_VERSION: &str = "2026-11-17";
/// Transformation pipeline version (normalization code version).
pub const TRANSFORMATION_VERSION: &str = "1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DataQuality {
    /// Validation warnings gathered while building the payload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_warnings: Vec<String>,
    /// Whether the raw MCP JSON is included alongside the normalized form.
    pub raw_available: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SyncMetadata {
    /// History checkpoint after this message (broker time, verbatim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// Sync batch kind when this is a synchronization message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageEnvelope {
    pub protocol_version: String,
    pub schema_version: String,
    /// Unique per message (uuid v7 — time ordered).
    pub message_id: String,
    /// Links this message to a triggering request/command, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// e.g. `sync.batch`, `action.result`, `heartbeat`, `manifest.update`.
    pub event_type: String,
    /// Connector-side time (RFC 3339).
    pub event_timestamp: String,
    pub device_id: String,
    /// Server-resolved identity (never client-asserted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    /// The MCP tool this data came from, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tool: Option<String>,
    pub payload: Value,
    pub data_quality: DataQuality,
    #[serde(default)]
    pub sync_metadata: SyncMetadata,
}

/// Identity + correlation context for building an envelope (keeps the
/// constructor small; fields mirror spec §6).
#[derive(Debug, Clone, Copy)]
pub struct MessageContext<'a> {
    pub event_type: &'a str,
    pub device_id: &'a str,
    /// Server-resolved identity — never client-asserted.
    pub user_id: Option<&'a str>,
    pub account_id: &'a str,
    pub broker_id: Option<&'a str>,
    pub terminal_id: Option<&'a str>,
    pub source_tool: Option<&'a str>,
    pub correlation_id: Option<&'a str>,
}

impl MessageEnvelope {
    /// Builds a message envelope from a context + payload.
    pub fn new(
        ctx: MessageContext<'_>,
        payload: Value,
        data_quality: DataQuality,
        sync_metadata: SyncMetadata,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_string(),
            schema_version: SCHEMA_VERSION.to_string(),
            message_id: uuid::Uuid::now_v7().to_string(),
            correlation_id: ctx.correlation_id.map(str::to_string),
            event_type: ctx.event_type.to_string(),
            event_timestamp: now_rfc3339(),
            device_id: ctx.device_id.to_string(),
            user_id: ctx.user_id.map(str::to_string),
            account_id: ctx.account_id.to_string(),
            broker_id: ctx.broker_id.map(str::to_string),
            terminal_id: ctx.terminal_id.map(str::to_string),
            source_tool: ctx.source_tool.map(str::to_string),
            payload,
            data_quality,
            sync_metadata,
        }
    }

    /// Wraps a sync batch produced by the engine.
    pub fn for_sync(
        batch: &crate::sync::SyncBatch,
        device_id: &str,
        user_id: Option<&str>,
        account_id: &str,
        broker_id: Option<&str>,
        checkpoint: Option<&str>,
        payload: Value,
    ) -> Self {
        Self::new(
            MessageContext {
                event_type: "sync.batch",
                device_id,
                user_id,
                account_id,
                broker_id,
                terminal_id: None,
                source_tool: Some(batch.kind.as_str()),
                correlation_id: Some(&batch.batch_id),
            },
            payload,
            DataQuality {
                validation_warnings: Vec::new(),
                raw_available: true,
            },
            SyncMetadata {
                checkpoint: checkpoint.map(str::to_string),
                batch_kind: Some(batch.kind.as_str().to_string()),
            },
        )
    }

    /// Wraps a dispatch outcome (raw MCP result preserved inside).
    pub fn for_action_result(
        outcome: &crate::actions::dispatcher::DispatchOutcome,
        device_id: &str,
        user_id: Option<&str>,
        account_id: &str,
        broker_id: Option<&str>,
        payload: Value,
    ) -> Self {
        Self::new(
            MessageContext {
                event_type: "action.result",
                device_id,
                user_id,
                account_id,
                broker_id,
                terminal_id: None,
                source_tool: Some(outcome.action_name.as_str()),
                correlation_id: Some(&outcome.correlation_id),
            },
            payload,
            DataQuality {
                validation_warnings: outcome.validation_errors.clone(),
                raw_available: outcome.actual_mcp_result.is_some(),
            },
            SyncMetadata::default(),
        )
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_carries_versions_and_identity() {
        let e = MessageEnvelope::new(
            MessageContext {
                event_type: "sync.batch",
                device_id: "device-1",
                user_id: Some("user-1"),
                account_id: "12345678901",
                broker_id: Some("MetaQuotes Ltd."),
                terminal_id: None,
                source_tool: Some("get_trading_account_info"),
                correlation_id: Some("corr-1"),
            },
            json!({ "balance": 26237.73 }),
            DataQuality {
                validation_warnings: vec![],
                raw_available: true,
            },
            SyncMetadata::default(),
        );
        assert_eq!(e.protocol_version, "1.0");
        assert_eq!(e.schema_version, "2026-11-17");
        assert_eq!(e.user_id.as_deref(), Some("user-1"));
        assert_eq!(e.correlation_id.as_deref(), Some("corr-1"));
        // message_id is a uuid v7 (time-ordered).
        let parsed = uuid::Uuid::parse_str(&e.message_id).expect("uuid");
        assert_eq!(parsed.get_version_num(), 7);
        // Serializes with all required §6 fields.
        let wire = serde_json::to_value(&e).expect("serialize");
        for field in [
            "protocol_version",
            "schema_version",
            "message_id",
            "event_type",
            "event_timestamp",
            "device_id",
            "account_id",
            "payload",
            "data_quality",
        ] {
            assert!(wire.get(field).is_some(), "missing §6 field: {field}");
        }
    }

    #[test]
    fn for_action_result_preserves_raw_result_flag() {
        let outcome = crate::actions::dispatcher::DispatchOutcome {
            request_id: "r1".into(),
            command_id: "c1".into(),
            action_name: "get_trading_account_info".into(),
            status: crate::actions::dispatcher::ActionStatus::Executed,
            started_at: "t".into(),
            completed_at: "t".into(),
            duration_ms: 5,
            actual_mcp_result: Some(json!({ "account": {} })),
            validation_errors: vec![],
            execution_error: None,
            correlation_id: "r1".into(),
        };
        let e = MessageEnvelope::for_action_result(
            &outcome,
            "device-1",
            None,
            "12345678901",
            None,
            json!({}),
        );
        assert_eq!(e.event_type, "action.result");
        assert!(e.data_quality.raw_available);
        assert_eq!(e.source_tool.as_deref(), Some("get_trading_account_info"));
    }
}
