//! Local command validation for server-delivered trade commands (spec §7).
//!
//! Hard rules implemented here:
//! - Explicit allowlist: only `close_position` and `modify_sl_tp` exist.
//!   Market/pending order placement is NOT in the allowlist, so auto-trade
//!   execution cannot be smuggled through a parsed command, and arbitrary
//!   operations (e.g. `send_web_request`, `run_program`) are rejected before
//!   they ever reach MT5.
//! - Commands expire (`expires_at`, RFC 3339).
//! - Replay protection via a bounded id cache.
//! - Device and account binding: a command for another device or another
//!   MT5 account is rejected (cross-account isolation).

use std::collections::{HashSet, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Operations the Connector can execute locally. This enum is the allowlist.
/// Anything the server sends that does not deserialize into one of these
/// variants is rejected as `UnsupportedOperation` or `Malformed`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandOperation {
    ClosePosition {
        ticket: String,
    },
    ModifySlTp {
        ticket: String,
        #[serde(default)]
        sl: Option<f64>,
        #[serde(default)]
        tp: Option<f64>,
    },
}

impl CommandOperation {
    /// Human-readable name for diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            CommandOperation::ClosePosition { .. } => "close_position",
            CommandOperation::ModifySlTp { .. } => "modify_sl_tp",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandEnvelope {
    pub command_id: String,
    /// MT5 login the command applies to, as authorized by NSE Core.
    pub account_id: String,
    pub device_id: String,
    pub operation: serde_json::Value,
    pub issued_at: String,
    pub expires_at: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRejection {
    Expired,
    Replayed,
    WrongDevice,
    WrongAccount,
    UnsupportedOperation,
    AutoTradeNotEnabled,
    Malformed,
}

impl fmt::Display for CommandRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CommandRejection::Expired => "command expired",
            CommandRejection::Replayed => "command replayed (idempotency)",
            CommandRejection::WrongDevice => "command bound to another device",
            CommandRejection::WrongAccount => "command bound to another account",
            CommandRejection::UnsupportedOperation => "operation not on the local allowlist",
            CommandRejection::AutoTradeNotEnabled => "feature entitlement missing locally",
            CommandRejection::Malformed => "command envelope malformed",
        };
        f.write_str(s)
    }
}

/// Validates server commands against the local allowlist and binding rules.
/// A successful validation returns the parsed operation; delivery alone is
/// never proof of execution.
pub struct CommandValidator {
    device_id: String,
    authorized_accounts: HashSet<String>,
    replay_cache: BoundedIdSet,
}

impl CommandValidator {
    pub fn new<I, S>(device_id: impl Into<String>, authorized_accounts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            device_id: device_id.into(),
            authorized_accounts: authorized_accounts.into_iter().map(Into::into).collect(),
            replay_cache: BoundedIdSet::new(10_000),
        }
    }

    /// Validates a raw JSON frame. Validation order: malformed → expired →
    /// device binding → account binding → allowlist → replay.
    pub fn validate(
        &mut self,
        raw: &str,
        feature_enabled: bool,
    ) -> Result<CommandOperation, CommandRejection> {
        let envelope: CommandEnvelope =
            serde_json::from_str(raw).map_err(|_| CommandRejection::Malformed)?;

        // 1. Expiration.
        let expires = OffsetDateTime::parse(&envelope.expires_at, &Rfc3339)
            .map_err(|_| CommandRejection::Malformed)?;
        if expires <= OffsetDateTime::now_utc() {
            return Err(CommandRejection::Expired);
        }

        // 2. Device binding.
        if envelope.device_id != self.device_id {
            return Err(CommandRejection::WrongDevice);
        }

        // 3. Account binding (cross-account isolation).
        if !self.authorized_accounts.contains(&envelope.account_id) {
            return Err(CommandRejection::WrongAccount);
        }

        // 4. Explicit allowlist: reject unknown operation kinds *before*
        //    deserializing them, so unsupported types never execute.
        let op_type = envelope
            .operation
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or(CommandRejection::UnsupportedOperation)?;
        if op_type != "close_position" && op_type != "modify_sl_tp" {
            return Err(CommandRejection::UnsupportedOperation);
        }

        // 5. Feature entitlement gate (position manager / auto trade off =>
        //    no execution, ever).
        if !feature_enabled {
            return Err(CommandRejection::AutoTradeNotEnabled);
        }

        // 6. Replay protection: only successful validations consume an id.
        let operation: CommandOperation =
            serde_json::from_value(envelope.operation).map_err(|_| CommandRejection::Malformed)?;
        if !self.replay_cache.insert(&envelope.command_id) {
            return Err(CommandRejection::Replayed);
        }
        Ok(operation)
    }
}

/// Bounded replay-protection cache.
struct BoundedIdSet {
    capacity: usize,
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl BoundedIdSet {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, id: &str) -> bool {
        if self.seen.contains(id) {
            return false;
        }
        while self.seen.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.seen.insert(id.to_string());
        self.order.push_back(id.to_string());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn envelope(
        op: Value,
        expires_at: &str,
        device: &str,
        account: &str,
        command_id: &str,
    ) -> String {
        json!({
            "command_id": command_id,
            "account_id": account,
            "device_id": device,
            "operation": op,
            "issued_at": "2026-11-17T00:00:00Z",
            "expires_at": expires_at,
            "idempotency_key": command_id,
        })
        .to_string()
    }

    fn validator() -> CommandValidator {
        CommandValidator::new("dev-1", ["12345678901".to_string()])
    }

    #[test]
    fn allowlisted_operations_pass_when_enabled() {
        let mut v = validator();
        let raw = envelope(
            json!({"type": "modify_sl_tp", "ticket": "123", "sl": 1.1, "tp": null}),
            "2099-01-01T00:00:00Z",
            "dev-1",
            "12345678901",
            "cmd-1",
        );
        let op = v.validate(&raw, true).expect("must validate");
        assert_eq!(op.kind(), "modify_sl_tp");
    }

    #[test]
    fn unsupported_operations_never_execute() {
        let mut v = validator();
        for op in ["send_web_request", "run_program", "trade_send_market_order"] {
            let raw = envelope(
                json!({"type": op, "url": "http://attacker"}),
                "2099-01-01T00:00:00Z",
                "dev-1",
                "12345678901",
                "cmd-x",
            );
            assert_eq!(
                v.validate(&raw, true).unwrap_err(),
                CommandRejection::UnsupportedOperation,
                "{op} must be rejected"
            );
        }
    }

    #[test]
    fn feature_disabled_blocks_execution() {
        let mut v = validator();
        let raw = envelope(
            json!({"type": "close_position", "ticket": "1"}),
            "2099-01-01T00:00:00Z",
            "dev-1",
            "12345678901",
            "cmd-2",
        );
        assert_eq!(
            v.validate(&raw, false).unwrap_err(),
            CommandRejection::AutoTradeNotEnabled
        );
    }

    #[test]
    fn cross_device_and_cross_account_are_rejected() {
        let mut v = validator();
        let wrong_device = envelope(
            json!({"type": "close_position", "ticket": "1"}),
            "2099-01-01T00:00:00Z",
            "dev-2",
            "12345678901",
            "cmd-3",
        );
        assert_eq!(
            v.validate(&wrong_device, true).unwrap_err(),
            CommandRejection::WrongDevice
        );

        let wrong_account = envelope(
            json!({"type": "close_position", "ticket": "1"}),
            "2099-01-01T00:00:00Z",
            "dev-1",
            "99999",
            "cmd-4",
        );
        assert_eq!(
            v.validate(&wrong_account, true).unwrap_err(),
            CommandRejection::WrongAccount
        );
    }

    #[test]
    fn expired_commands_are_rejected() {
        let mut v = validator();
        let raw = envelope(
            json!({"type": "close_position", "ticket": "1"}),
            "2000-01-01T00:00:00Z",
            "dev-1",
            "12345678901",
            "cmd-5",
        );
        assert_eq!(
            v.validate(&raw, true).unwrap_err(),
            CommandRejection::Expired
        );
    }

    #[test]
    fn replayed_commands_are_rejected() {
        let mut v = validator();
        let raw = envelope(
            json!({"type": "close_position", "ticket": "1"}),
            "2099-01-01T00:00:00Z",
            "dev-1",
            "12345678901",
            "cmd-6",
        );
        assert!(v.validate(&raw, true).is_ok());
        assert_eq!(
            v.validate(&raw, true).unwrap_err(),
            CommandRejection::Replayed
        );
    }
}
