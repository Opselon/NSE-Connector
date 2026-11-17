//! Incoming command frames — reconciles the two wire dialects:
//!
//! **As-built Platform dialect** (verified in the deployed server code,
//! `connector/commands.rs::CommandFrame::to_json`): the payload lives inside
//! an `operation` OBJECT whose `type` field is the envelope name and whose
//! remaining keys are the parameters. There is no `request_id` and no
//! separate `parameters` field; `idempotency_key` == `command_id`.
//!
//! ```json
//! {"type":"command","command_id":"<uuid>","account_id":"<login>",
//!  "device_id":"<device>","operation":{"type":"modify_sl_tp",
//!  "symbol":"EURUSD","position_ticket":"123","sl":1.08,"tp":1.09},
//!  "issued_at":"...","expires_at":"...","idempotency_key":"<uuid>"}
//! ```
//!
//! **Proposed-contract dialect** (docs/PROPOSED_NSE_PLATFORM_INTEGRATION.md
//! §6.2, used by the mock suite): `action_name` + `parameters` + optional
//! `request_id` + `authorization_context`.
//!
//! Both parse into the same [`IncomingCommand`], which the daemon gates and
//! executes through the dispatcher / bridge registry. Unknown envelopes are
//! resolved to the registry operation name itself when it looks like a known
//! action — the manifest gate rejects anything the connector cannot run.

use serde_json::{Value, json};

use crate::actions::dispatcher::ActionRequest;

/// A parsed, dialect-normalized command frame.
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingCommand {
    pub command_id: String,
    /// Proposed dialect only; the as-built server has no request id and the
    /// command_id is the correlation id.
    pub request_id: Option<String>,
    pub account_id: String,
    pub device_id: String,
    pub issued_at: String,
    pub expires_at: String,
    pub idempotency_key: String,
    /// Resolved executor name: an MCP manifest action or a bridge command.
    pub action_name: String,
    pub parameters: Value,
    pub authorization_context: Value,
}

impl IncomingCommand {
    /// Parses one `command` frame in either dialect. Returns `Err` with a
    /// short reason for frames that cannot be routed at all.
    ///
    /// # Errors
    /// Missing `command_id`/`account_id`/`device_id`, or an as-built frame
    /// whose `operation` is not an object.
    pub fn parse(frame: &Value) -> Result<Self, String> {
        let command_id = frame
            .get("command_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing command_id".to_string())?
            .to_string();
        let account_id = frame
            .get("account_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing account_id".to_string())?
            .to_string();
        let device_id = frame
            .get("device_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing device_id".to_string())?
            .to_string();
        let issued_at = frame
            .get("issued_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let expires_at = frame
            .get("expires_at")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing expires_at".to_string())?
            .to_string();

        if let Some(operation) = frame.get("operation") {
            // ---- As-built dialect ----
            let op_obj = operation
                .as_object()
                .ok_or_else(|| "as-built 'operation' must be an object".to_string())?;
            let envelope = op_obj
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| "operation.type (envelope) missing".to_string())?
                .to_string();
            let mut parameters = operation.clone();
            if let Some(obj) = parameters.as_object_mut() {
                obj.remove("type");
            }
            let (action_name, parameters) = resolve_envelope(&envelope, &parameters);
            Ok(Self {
                request_id: None,
                idempotency_key: frame
                    .get("idempotency_key")
                    .and_then(Value::as_str)
                    .unwrap_or(&command_id)
                    .to_string(),
                command_id,
                account_id,
                device_id,
                issued_at,
                expires_at,
                action_name,
                parameters,
                authorization_context: frame
                    .get("authorization_context")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            })
        } else if let Some(action_name) = frame.get("action_name").and_then(Value::as_str) {
            // ---- Proposed dialect ----
            Ok(Self {
                request_id: frame
                    .get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                idempotency_key: frame
                    .get("idempotency_key")
                    .and_then(Value::as_str)
                    .unwrap_or(&command_id)
                    .to_string(),
                command_id,
                account_id,
                device_id,
                issued_at,
                expires_at,
                action_name: action_name.to_string(),
                parameters: frame
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
                authorization_context: frame
                    .get("authorization_context")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            })
        } else {
            Err("frame carries neither 'operation' (as-built) nor 'action_name' (proposed)".into())
        }
    }

    /// Normalizes into an [`ActionRequest`] for the shared gate chain
    /// (dispatcher / bridge). As-built frames synthesize request_id from the
    /// command id.
    pub fn to_action_request(&self) -> ActionRequest {
        ActionRequest {
            request_id: self
                .request_id
                .clone()
                .unwrap_or_else(|| self.command_id.clone()),
            command_id: self.command_id.clone(),
            action_name: self.action_name.clone(),
            account_id: self.account_id.clone(),
            device_id: self.device_id.clone(),
            issued_at: self.issued_at.clone(),
            expires_at: self.expires_at.clone(),
            idempotency_key: self.idempotency_key.clone(),
            parameters: self.parameters.clone(),
            authorization_context: self.authorization_context.clone(),
        }
    }
}

/// Maps an as-built envelope name to the concrete executor the connector
/// runs, stripping the envelope discriminator from the parameters.
///
/// Today's server allowlist is two envelopes (close_position family and
/// modify_sl_tp family, 9 registry operations). The mapping stays narrow and
/// explicit — anything else passes through as-is so the manifest gate can
/// reject it with a clear reason instead of the connector guessing.
fn resolve_envelope(envelope: &str, params: &Value) -> (String, Value) {
    match envelope {
        "modify_sl_tp" => ("trade_modify_sl_tp".to_string(), params.clone()),
        "close_position" => {
            let has_by = params.get("position_ticket_by").is_some();
            let has_ticket = params.get("position_ticket").is_some();
            if has_by {
                ("trade_close_by_position".to_string(), params.clone())
            } else if has_ticket {
                ("trade_close_single_position".to_string(), params.clone())
            } else {
                // No ticket → close everything for the account.
                ("trade_close_all_positions".to_string(), params.clone())
            }
        }
        // Not a known envelope — maybe the name IS a runnable action
        // (native MCP action or bridge command); the gates decide.
        other => (other.to_string(), params.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_as_built_operation_frame() {
        let frame = json!({
            "type": "command",
            "command_id": "0199abcd-0000-7000-8000-000000000001",
            "account_id": "12345678901",
            "device_id": "dev-1",
            "operation": {
                "type": "modify_sl_tp",
                "symbol": "EURUSD",
                "position_ticket": "152913954047",
                "sl": 1.083,
                "tp": 1.087
            },
            "issued_at": "2026-11-17T15:00:00Z",
            "expires_at": "2026-11-17T15:02:00Z",
            "idempotency_key": "0199abcd-0000-7000-8000-000000000001"
        });
        let cmd = IncomingCommand::parse(&frame).expect("parses");
        assert_eq!(cmd.action_name, "trade_modify_sl_tp");
        assert_eq!(cmd.parameters["symbol"], json!("EURUSD"));
        assert!(
            cmd.parameters.get("type").is_none(),
            "envelope discriminator stripped"
        );
        assert_eq!(cmd.request_id, None);
        let req = cmd.to_action_request();
        assert_eq!(req.request_id, req.command_id);
        assert_eq!(req.idempotency_key, req.command_id);
    }

    #[test]
    fn parses_proposed_action_name_frame() {
        let frame = json!({
            "type": "command",
            "request_id": "req-1",
            "command_id": "cmd-1",
            "action_name": "trade_send_market_order",
            "account_id": "12345678901",
            "device_id": "dev-1",
            "issued_at": "2026-11-17T15:00:00Z",
            "expires_at": "2026-11-17T15:02:00Z",
            "idempotency_key": "k-1",
            "parameters": { "symbol": "EURUSD", "type": "buy", "volume": 0.01 }
        });
        let cmd = IncomingCommand::parse(&frame).expect("parses");
        assert_eq!(cmd.action_name, "trade_send_market_order");
        assert_eq!(cmd.request_id.as_deref(), Some("req-1"));
        assert_eq!(cmd.idempotency_key, "k-1");
        assert_eq!(cmd.parameters["volume"], json!(0.01));
    }

    #[test]
    fn close_envelope_resolves_by_ticket_shape() {
        let mk = |params: Value| {
            let frame = json!({
                "type": "command", "command_id": "c", "account_id": "a",
                "device_id": "d", "operation": params,
                "issued_at": "t", "expires_at": "e"
            });
            IncomingCommand::parse(&frame).expect("parses").action_name
        };
        assert_eq!(
            mk(json!({ "type": "close_position", "symbol": "EURUSD", "position_ticket": "1" })),
            "trade_close_single_position"
        );
        assert_eq!(
            mk(json!({ "type": "close_position", "symbol": "EURUSD",
                       "position_ticket": "1", "position_ticket_by": "2" })),
            "trade_close_by_position"
        );
        assert_eq!(
            mk(json!({ "type": "close_position", "symbol": "EURUSD" })),
            "trade_close_all_positions"
        );
    }

    #[test]
    fn unknown_envelope_passes_through_for_the_manifest_gate() {
        let frame = json!({
            "type": "command", "command_id": "c", "account_id": "a",
            "device_id": "d", "operation": { "type": "future_envelope", "x": 1 },
            "issued_at": "t", "expires_at": "e"
        });
        let cmd = IncomingCommand::parse(&frame).expect("parses");
        assert_eq!(cmd.action_name, "future_envelope");
        assert_eq!(cmd.parameters["x"], json!(1));
    }

    #[test]
    fn malformed_frames_report_a_reason() {
        let no_operation = json!({
            "type": "command", "command_id": "c", "account_id": "a",
            "device_id": "d", "issued_at": "t", "expires_at": "e"
        });
        assert!(IncomingCommand::parse(&no_operation).is_err());
        let no_id = json!({
            "type": "command", "account_id": "a", "device_id": "d",
            "operation": { "type": "modify_sl_tp" }, "expires_at": "e"
        });
        assert!(IncomingCommand::parse(&no_id).is_err());
    }
}
