//! Tolerant MT5 models.
//!
//! Field names verified LIVE on 2026-11-17 against a real
//! `MetaTrader 5 MCP` v1.0 server (build 6306) with an actual open position
//! and deal in history:
//! - `get_trading_open_positions` → `positions[]` uses `position_id`,
//!   `action`, `create_time`/`update_time`, `price_open`, `price_last`,
//!   `volume`, `profit`, `contract_size`, `reason`, `comment`.
//! - `get_trading_history_orders` → `orders[]` uses `order_id`,
//!   `position_id`, `type`, `filling`, `state`, `open_time`/`done_time`,
//!   `volume_initial`/`volume_current`; `deals[]` uses `deal_id`,
//!   `entry`, `action`, `open_time`, `price_open`, `volume`, `profit`.
//! - Timestamps are ISO-8601 WITHOUT a timezone suffix
//!   (`2026-11-16T22:33:39`) — preserved verbatim; see
//!   `parse_broker_time` for ordering.
//!
//! Broker/MCP-server variation law: other MT5 MCP implementations or
//! brokers may use different names, so earlier/alternative names are kept
//! as serde aliases and every record field stays optional. Nothing is
//! invented as mandatory.

use serde::{Deserialize, Serialize};

/// Deserializes a JSON string or number into a `String` (MT5 tickets arrive
/// as either). Returns `None` for JSON null.
pub mod flex_string {
    use serde::{Deserialize, Deserializer, de::Error as _};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::Null => Ok(None),
            serde_json::Value::String(s) => Ok(Some(s)),
            serde_json::Value::Number(n) => Ok(Some(n.to_string())),
            other => Err(D::Error::custom(format!(
                "expected string or number, got: {other}"
            ))),
        }
    }
}

/// Account snapshot as returned by `get_trading_account_info` (verified
/// live). The `terminal` block carries broker-time context
/// (`terminal.timestamp`, `connection.state`) which must be preserved, not
/// re-localized.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountSnapshot {
    pub account: AccountInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalInfo>,
}

/// `account` object — all fields optional so partial information is
/// tolerated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AccountInfo {
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub broker: Option<String>,
    /// MT5 login number. Masked in all diagnostics (`security::mask_login`).
    #[serde(default, deserialize_with = "flex_string::deserialize")]
    pub login: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// `demo` | `live` | `contest` (as reported by the server).
    #[serde(default, rename = "type")]
    pub account_type: Option<String>,
    #[serde(default)]
    pub read_only: Option<bool>,
    #[serde(default)]
    pub margin_mode: Option<String>,
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub credit: Option<f64>,
    #[serde(default)]
    pub margin: Option<f64>,
    #[serde(default)]
    pub margin_free: Option<f64>,
    #[serde(default)]
    pub profit: Option<f64>,
    #[serde(default)]
    pub equity: Option<f64>,
    #[serde(default)]
    pub swaps: Option<f64>,
    #[serde(default)]
    pub commissions: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TerminalInfo {
    /// Broker/terminal timestamp, preserved verbatim.
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub connection: Option<TerminalConnection>,
    #[serde(default)]
    pub build: Option<u64>,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub program_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TerminalConnection {
    /// e.g. `synchronized` on the live server.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub ping_microseconds: Option<u64>,
    #[serde(default)]
    pub last_authorized_at: Option<String>,
    #[serde(default)]
    pub last_connection_lost_at: Option<String>,
}

/// `get_trading_open_positions` envelope (verified live: `{"positions":
/// [...], "orders": [...]}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct OpenPositionsResponse {
    #[serde(default)]
    pub positions: Vec<Mt5Position>,
    #[serde(default)]
    pub orders: Vec<Mt5Order>,
}

/// `get_trading_history_orders` envelope (verified live: `{"orders":
/// [...], "deals": [...]}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HistoryOrdersResponse {
    #[serde(default)]
    pub orders: Vec<Mt5Order>,
    #[serde(default)]
    pub deals: Vec<Mt5Deal>,
}

/// `get_trading_history_positions` envelope (verified live: `{"positions":
/// [...]}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HistoryPositionsResponse {
    #[serde(default)]
    pub positions: Vec<Mt5HistoryPosition>,
}

/// Open position record — field names verified live.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Mt5Position {
    /// Live name: `position_id` (string even though semantically numeric).
    #[serde(
        default,
        rename = "position_id",
        alias = "ticket",
        deserialize_with = "flex_string::deserialize"
    )]
    pub position_id: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    /// Live name: `action` — `buy` / `sell`.
    #[serde(default, rename = "action", alias = "direction")]
    pub action: Option<String>,
    /// e.g. `Client` (verified live).
    #[serde(default)]
    pub reason: Option<String>,
    /// Live name: `create_time` — no timezone suffix (terminal-local).
    #[serde(default, alias = "opened_at", alias = "open_time", alias = "time")]
    pub create_time: Option<String>,
    #[serde(default, alias = "update_time")]
    pub update_time: Option<String>,
    #[serde(default, alias = "volume_lots")]
    pub volume: Option<f64>,
    #[serde(default, alias = "entry_price")]
    pub price_open: Option<f64>,
    /// Live name: `price_last` (current price).
    #[serde(default, alias = "price_current")]
    pub price_last: Option<f64>,
    #[serde(default, alias = "stop_loss")]
    pub sl: Option<f64>,
    #[serde(default, alias = "take_profit")]
    pub tp: Option<f64>,
    /// Floating profit/loss in account currency.
    #[serde(default)]
    pub profit: Option<f64>,
    #[serde(default)]
    pub swap: Option<f64>,
    #[serde(default)]
    pub commission: Option<f64>,
    /// e.g. `100000.0` (verified live).
    #[serde(default)]
    pub contract_size: Option<f64>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default, alias = "magic_number")]
    pub magic: Option<u64>,
}

impl Mt5Position {
    /// Records without an id cannot be reconciled server-side.
    pub fn is_reconcilable(&self) -> bool {
        self.position_id.is_some()
    }
}

/// Order record (open or historical) — field names verified live for
/// history orders; open orders share the shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Mt5Order {
    /// Live name: `order_id`.
    #[serde(
        default,
        rename = "order_id",
        alias = "ticket",
        deserialize_with = "flex_string::deserialize"
    )]
    pub order_id: Option<String>,
    /// External/broker-side id (verified live, may be empty string).
    #[serde(default, alias = "external_id")]
    pub order_external_id: Option<String>,
    #[serde(default, alias = "position_ticket", alias = "position")]
    pub position_id: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    /// `buy` / `sell` (live: `type`).
    #[serde(default, rename = "type", alias = "direction", alias = "side")]
    pub order_action: Option<String>,
    /// Order kind for pending orders: `limit` / `stop` / `stop_limit`.
    #[serde(default, alias = "order_kind")]
    pub order_type: Option<String>,
    /// Live: `filling` — e.g. `fill or kill`.
    #[serde(default, alias = "filling_mode")]
    pub filling: Option<String>,
    /// Live: `state` — e.g. `filled`.
    #[serde(default, alias = "status")]
    pub state: Option<String>,
    /// Live: `open_time` (no timezone suffix).
    #[serde(default, alias = "placed_at", alias = "time_setup", alias = "time")]
    pub open_time: Option<String>,
    /// Live: `done_time` (no timezone suffix).
    #[serde(default, alias = "done_at")]
    pub done_time: Option<String>,
    /// Live: `open_reason` — e.g. `Client`.
    #[serde(default)]
    pub open_reason: Option<String>,
    /// Live: `volume_initial`.
    #[serde(default, alias = "volume")]
    pub volume_initial: Option<f64>,
    /// Live: `volume_current`.
    #[serde(default)]
    pub volume_current: Option<f64>,
    #[serde(default)]
    pub contract_size: Option<f64>,
    #[serde(default, alias = "price")]
    pub price_open: Option<f64>,
    #[serde(default, alias = "price_current")]
    pub price_last: Option<f64>,
    #[serde(default, alias = "stop_loss")]
    pub sl: Option<f64>,
    #[serde(default, alias = "take_profit")]
    pub tp: Option<f64>,
    #[serde(default)]
    pub profit: Option<f64>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default, alias = "magic_number")]
    pub magic: Option<u64>,
}

impl Mt5Order {
    pub fn is_reconcilable(&self) -> bool {
        self.order_id.is_some()
    }
}

/// Deal record (executions) — field names verified live.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Mt5Deal {
    /// Live name: `deal_id`.
    #[serde(
        default,
        rename = "deal_id",
        alias = "ticket",
        alias = "deal",
        deserialize_with = "flex_string::deserialize"
    )]
    pub deal_id: Option<String>,
    #[serde(default, alias = "external_id")]
    pub deal_external_id: Option<String>,
    #[serde(default, alias = "order_ticket", alias = "order_id")]
    pub order_id: Option<String>,
    #[serde(default, alias = "position_ticket", alias = "position_id")]
    pub position_id: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    /// `buy` / `sell` (live: `action`).
    #[serde(default, rename = "action", alias = "direction", alias = "side")]
    pub deal_action: Option<String>,
    /// Entry kind — live: `in` / `out` / `inout`.
    #[serde(default, alias = "entry_type")]
    pub entry: Option<String>,
    /// e.g. `Client` (verified live).
    #[serde(default)]
    pub reason: Option<String>,
    /// Live name: `open_time` (no timezone suffix).
    #[serde(
        default,
        alias = "executed_at",
        alias = "time",
        alias = "execution_time"
    )]
    pub open_time: Option<String>,
    /// Live name: `price_open`.
    #[serde(
        default,
        alias = "price",
        alias = "price_executed",
        alias = "exec_price"
    )]
    pub price_open: Option<f64>,
    #[serde(default)]
    pub volume: Option<f64>,
    /// Realized profit/loss.
    #[serde(default)]
    pub profit: Option<f64>,
    #[serde(default)]
    pub commission: Option<f64>,
    #[serde(default)]
    pub swap: Option<f64>,
    #[serde(default)]
    pub fee: Option<f64>,
    #[serde(default)]
    pub contract_size: Option<f64>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default, alias = "magic_number")]
    pub magic: Option<u64>,
}

impl Mt5Deal {
    pub fn is_reconcilable(&self) -> bool {
        self.deal_id.is_some()
    }
}

/// Aggregated closed position (reconstructed from deals). Field names not
/// yet verified live (the test account had no closed positions at capture
/// time) — tolerant shape driving daily P/L analytics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Mt5HistoryPosition {
    #[serde(
        default,
        alias = "position_id",
        alias = "ticket",
        deserialize_with = "flex_string::deserialize"
    )]
    pub position_id: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default, alias = "type", alias = "action", alias = "direction")]
    pub action: Option<String>,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default, alias = "price_open", alias = "entry_price")]
    pub open_price: Option<f64>,
    #[serde(default, alias = "price_close")]
    pub close_price: Option<f64>,
    #[serde(default, alias = "stop_loss")]
    pub sl: Option<f64>,
    #[serde(default, alias = "take_profit")]
    pub tp: Option<f64>,
    #[serde(default)]
    pub profit: Option<f64>,
    #[serde(default)]
    pub swap: Option<f64>,
    #[serde(default)]
    pub commission: Option<f64>,
    #[serde(default)]
    pub fee: Option<f64>,
    #[serde(default, alias = "create_time", alias = "open_time")]
    pub opened_at: Option<String>,
    #[serde(default, alias = "close_time", alias = "done_time", alias = "time")]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default, alias = "magic_number")]
    pub magic: Option<u64>,
}

impl Mt5HistoryPosition {
    pub fn is_reconcilable(&self) -> bool {
        self.position_id.is_some()
    }
}

/// Parses a broker/terminal timestamp for ordering purposes.
///
/// The live server emits ISO-8601 WITHOUT a timezone suffix
/// (`2026-11-16T22:33:39`); full RFC 3339 is also accepted. Timezone-less
/// values are interpreted as UTC *for ordering only* — the verbatim string
/// is always what gets transmitted, and records from one server use one
/// consistent format, so ordering is stable.
pub fn parse_broker_time(s: &str) -> Option<time::OffsetDateTime> {
    use time::format_description::well_known::Rfc3339;
    let no_tz = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    time::OffsetDateTime::parse(s, &Rfc3339)
        .or_else(|_| time::PrimitiveDateTime::parse(s, &no_tz).map(|p| p.assume_utc()))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_broker_time_handles_both_formats() {
        // Live-verified shape: no timezone suffix.
        let naive = parse_broker_time("2026-11-16T22:33:39").expect("no-tz format");
        assert_eq!(
            naive
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            "2026-11-16T22:33:39Z"
        );
        // Full RFC 3339 (terminal.timestamp uses it).
        let full = parse_broker_time("2026-11-16T20:31:21Z").expect("rfc3339");
        assert_ne!(full, naive);
        assert_eq!(parse_broker_time("not a time"), None);
    }

    #[test]
    fn real_position_payload_maps() {
        // Captured live (IDs sanitized).
        let raw = serde_json::json!({
            "position_id": "152913954047",
            "action": "buy",
            "reason": "Client",
            "symbol": "EURUSD",
            "create_time": "2026-11-16T22:33:39",
            "update_time": "2026-11-16T22:33:39",
            "volume": 0.01,
            "price_open": 1.09995,
            "price_last": 1.09951,
            "profit": -0.44,
            "contract_size": 100000.0,
            "comment": ""
        });
        let p: Mt5Position = serde_json::from_value(raw).expect("live shape");
        assert_eq!(p.position_id.as_deref(), Some("152913954047"));
        assert_eq!(p.action.as_deref(), Some("buy"));
        assert_eq!(p.price_last, Some(1.09951));
        assert!(p.is_reconcilable());
    }

    #[test]
    fn real_deal_payload_maps() {
        let raw = serde_json::json!({
            "deal_id": "152756177827",
            "deal_external_id": "",
            "order_id": "152913954047",
            "position_id": "152913954047",
            "symbol": "EURUSD",
            "action": "buy",
            "entry": "in",
            "reason": "Client",
            "open_time": "2026-11-16T22:33:39",
            "price_open": 1.09995,
            "volume": 0.01,
            "profit": 0.0,
            "contract_size": 100000.0,
            "comment": ""
        });
        let d: Mt5Deal = serde_json::from_value(raw).expect("live shape");
        assert_eq!(d.deal_id.as_deref(), Some("152756177827"));
        assert_eq!(d.entry.as_deref(), Some("in"));
        assert_eq!(d.open_time.as_deref(), Some("2026-11-16T22:33:39"));
        assert!(d.is_reconcilable());
    }

    #[test]
    fn real_history_order_payload_maps() {
        let raw = serde_json::json!({
            "order_id": "152913954047",
            "order_external_id": "",
            "position_id": "152913954047",
            "symbol": "EURUSD",
            "type": "buy",
            "filling": "fill or kill",
            "open_time": "2026-11-16T22:33:39",
            "done_time": "2026-11-16T22:33:39",
            "open_reason": "Client",
            "state": "filled",
            "volume_initial": 0.01,
            "volume_current": 0,
            "contract_size": 100000.0,
            "comment": ""
        });
        let o: Mt5Order = serde_json::from_value(raw).expect("live shape");
        assert_eq!(o.order_id.as_deref(), Some("152913954047"));
        assert_eq!(o.state.as_deref(), Some("filled"));
        assert_eq!(o.volume_current, Some(0.0));
        assert!(o.is_reconcilable());
    }
}
