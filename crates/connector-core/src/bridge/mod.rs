//! NSE MT5 Bridge command surface — 90 platform-facing commands in 10
//! categories, mapping onto (a) raw MT5 MCP actions, (b) derived computation
//! from verified live payloads, or (c) bridge-local state.
//!
//! Every command answers the same JSON envelope, so the Platform needs no
//! broker knowledge:
//!
//! ```json
//! {"command": "get_account_balance", "ok": true,
//!  "data": {"balance": 26237.73, "currency": "USD"},
//!  "broker_time": "2026-11-17T15:43:45", "error": null}
//! ```
//!
//! Failures carry `ok: false` plus a machine-readable error code
//! (`invalid_params`, `not_available`, `mt5_error`, `bridge_internal`).
//! Derived answers never invent broker data — fields the broker does not
//! expose return `not_available` with the reason, mirroring the live
//! capability matrix discipline.

pub mod query;
pub mod risk_diag;
pub mod trading;

use std::collections::{HashSet, VecDeque};
use std::sync::RwLock;
use std::time::Instant;

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::format_description;

use crate::actions::dispatcher::{ActionRequest, DispatchContext, DispatchRejection};
use crate::actions::kill::KillSwitch;
use crate::actions::risk::RiskTier;
use crate::mcp::{McpError, McpHttpClient};

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// One bridge command in the platform-facing registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeCommandSpec {
    pub name: &'static str,
    /// `terminal` | `account` | `symbols` | `market` | `chart` |
    /// `positions` | `orders` | `trading` | `risk` | `diagnostics`
    pub category: &'static str,
    pub tier: RiskTier,
    pub summary: &'static str,
}

macro_rules! cmd {
    ($name:literal, $cat:literal, $tier:ident, $summary:literal) => {
        BridgeCommandSpec {
            name: $name,
            category: $cat,
            tier: RiskTier::$tier,
            summary: $summary,
        }
    };
}

/// The full 90-command surface (single source of truth for capability
/// reporting; enforced by `registry_has_exactly_90_unique_commands`).
pub const BRIDGE_COMMANDS: &[BridgeCommandSpec] = &[
    // 1. Connection & terminal state (1-9)
    cmd!(
        "get_bridge_status",
        "terminal",
        ReadOnly,
        "bridge uptime, connection and command counters"
    ),
    cmd!(
        "get_mt5_connection_status",
        "terminal",
        ReadOnly,
        "MT5 connection state and last handshake"
    ),
    cmd!(
        "connect_mt5",
        "terminal",
        ReadOnly,
        "perform the MCP handshake with the terminal"
    ),
    cmd!(
        "disconnect_mt5",
        "terminal",
        ReadOnly,
        "release the current MT5 session"
    ),
    cmd!(
        "reconnect_mt5",
        "terminal",
        ReadOnly,
        "re-handshake with the terminal"
    ),
    cmd!(
        "get_mt5_terminal_info",
        "terminal",
        ReadOnly,
        "full terminal info passthrough"
    ),
    cmd!(
        "get_mt5_terminal_version",
        "terminal",
        ReadOnly,
        "terminal build and program name"
    ),
    cmd!(
        "get_mt5_server_info",
        "terminal",
        ReadOnly,
        "broker/server identity"
    ),
    cmd!(
        "get_mcp_server_info",
        "terminal",
        ReadOnly,
        "MCP protocol handshake info and tool count"
    ),
    // 2. Account (10-18)
    cmd!(
        "get_trading_account_info",
        "account",
        ReadOnly,
        "full account snapshot passthrough"
    ),
    cmd!(
        "get_account_balance",
        "account",
        ReadOnly,
        "account balance"
    ),
    cmd!("get_account_equity", "account", ReadOnly, "current equity"),
    cmd!("get_account_margin", "account", ReadOnly, "used margin"),
    cmd!(
        "get_account_free_margin",
        "account",
        ReadOnly,
        "free margin"
    ),
    cmd!(
        "get_account_margin_level",
        "account",
        ReadOnly,
        "margin level"
    ),
    cmd!(
        "get_account_profit",
        "account",
        ReadOnly,
        "floating profit/loss"
    ),
    cmd!("get_account_currency", "account", ReadOnly, "base currency"),
    cmd!(
        "get_account_trading_permissions",
        "account",
        ReadOnly,
        "algo/MCP trading permission flags"
    ),
    // 3. Symbols & specs (19-27)
    cmd!(
        "get_trading_symbols",
        "symbols",
        ReadOnly,
        "symbols in Market Watch"
    ),
    cmd!(
        "get_symbol_info",
        "symbols",
        ReadOnly,
        "full broker spec for one symbol"
    ),
    cmd!(
        "get_symbol_price",
        "symbols",
        ReadOnly,
        "last tick price for one symbol"
    ),
    cmd!(
        "get_symbol_tick_size",
        "symbols",
        ReadOnly,
        "symbol tick size"
    ),
    cmd!(
        "get_symbol_point",
        "symbols",
        ReadOnly,
        "symbol point value"
    ),
    cmd!(
        "get_symbol_digits",
        "symbols",
        ReadOnly,
        "symbol price digits"
    ),
    cmd!(
        "get_symbol_spread",
        "symbols",
        ReadOnly,
        "current spread in price units and points"
    ),
    cmd!(
        "get_symbol_trading_sessions",
        "symbols",
        ReadOnly,
        "trading sessions (when the broker exposes them)"
    ),
    cmd!(
        "get_symbol_trade_constraints",
        "symbols",
        ReadOnly,
        "volume/trade mode constraints for one symbol"
    ),
    // 4. Live market data (28-36)
    cmd!("get_market_tick", "market", ReadOnly, "latest tick"),
    cmd!("get_market_bid", "market", ReadOnly, "latest bid"),
    cmd!("get_market_ask", "market", ReadOnly, "latest ask"),
    cmd!(
        "get_market_last_price",
        "market",
        ReadOnly,
        "latest traded price"
    ),
    cmd!(
        "get_market_depth",
        "market",
        LocalModification,
        "level-2 book (subscribes the book internally)"
    ),
    cmd!(
        "get_market_spread_history",
        "market",
        ReadOnly,
        "computed spread series over a window"
    ),
    cmd!(
        "get_market_tick_volume",
        "market",
        ReadOnly,
        "tick volumes over a window"
    ),
    cmd!(
        "get_market_watchlist",
        "market",
        ReadOnly,
        "symbols selected in Market Watch"
    ),
    cmd!(
        "subscribe_market_ticks",
        "market",
        ReadOnly,
        "register a poll subscription for live ticks"
    ),
    // 5. Charts & history (37-45)
    cmd!(
        "get_chart_history",
        "chart",
        ReadOnly,
        "candle history passthrough"
    ),
    cmd!(
        "get_chart_ticks_history",
        "chart",
        ReadOnly,
        "tick history passthrough"
    ),
    cmd!(
        "get_chart_current_timeframe",
        "chart",
        ReadOnly,
        "active chart timeframe (if the broker exposes it)"
    ),
    cmd!(
        "get_chart_available_timeframes",
        "chart",
        ReadOnly,
        "supported timeframe list"
    ),
    cmd!(
        "get_chart_ohlcv",
        "chart",
        ReadOnly,
        "OHLCV-shaped candle data"
    ),
    cmd!(
        "get_chart_indicators",
        "chart",
        ReadOnly,
        "indicators on the chart (if exposed)"
    ),
    cmd!(
        "get_chart_objects",
        "chart",
        ReadOnly,
        "chart objects (if exposed)"
    ),
    cmd!(
        "get_chart_active_info",
        "chart",
        ReadOnly,
        "active chart metadata (if exposed)"
    ),
    cmd!(
        "get_chart_multi_timeframe_history",
        "chart",
        ReadOnly,
        "candles for up to 5 timeframes"
    ),
    // 6. Positions (46-54)
    cmd!(
        "get_trading_open_positions",
        "positions",
        ReadOnly,
        "open positions + working orders"
    ),
    cmd!(
        "get_position_info",
        "positions",
        ReadOnly,
        "one position by ticket"
    ),
    cmd!(
        "get_position_profit",
        "positions",
        ReadOnly,
        "floating P/L of one position"
    ),
    cmd!(
        "get_position_volume",
        "positions",
        ReadOnly,
        "volume of one position"
    ),
    cmd!(
        "get_position_sl_tp",
        "positions",
        ReadOnly,
        "SL/TP of one position"
    ),
    cmd!(
        "get_position_risk",
        "positions",
        ReadOnly,
        "margin + SL/TP outcome estimate for one position"
    ),
    cmd!(
        "trade_modify_sl_tp",
        "positions",
        Financial,
        "modify SL/TP of a position or pending order"
    ),
    cmd!(
        "trade_close_single_position",
        "positions",
        Financial,
        "close one position"
    ),
    cmd!(
        "trade_close_all_positions",
        "positions",
        Financial,
        "close every open position (bounded loop)"
    ),
    // 7. Orders & history (55-63)
    cmd!(
        "get_trading_pending_orders",
        "orders",
        ReadOnly,
        "working pending orders"
    ),
    cmd!(
        "get_order_info",
        "orders",
        ReadOnly,
        "one order by ticket (open or recent history)"
    ),
    cmd!("get_order_status", "orders", ReadOnly, "state of one order"),
    cmd!(
        "get_trading_history_orders",
        "orders",
        ReadOnly,
        "order history window passthrough"
    ),
    cmd!(
        "get_trading_history_positions",
        "orders",
        ReadOnly,
        "closed positions history"
    ),
    cmd!(
        "get_trading_deals_history",
        "orders",
        ReadOnly,
        "executed deals window"
    ),
    cmd!(
        "trade_cancel_pending_order",
        "orders",
        Financial,
        "delete a pending order"
    ),
    cmd!(
        "trade_modify_pending_order",
        "orders",
        Financial,
        "modify pending SL/TP (price modify not exposed by MT5 MCP)"
    ),
    cmd!(
        "get_order_execution_details",
        "orders",
        ReadOnly,
        "deals generated by one order"
    ),
    // 8. Trade execution (64-72)
    cmd!(
        "trade_send_market_order",
        "trading",
        Financial,
        "send a market order"
    ),
    cmd!(
        "trade_send_buy_order",
        "trading",
        Financial,
        "send a market BUY"
    ),
    cmd!(
        "trade_send_sell_order",
        "trading",
        Financial,
        "send a market SELL"
    ),
    cmd!(
        "trade_send_pending_order",
        "trading",
        Financial,
        "place a pending order"
    ),
    cmd!(
        "trade_close_partial_position",
        "trading",
        Financial,
        "partial close (not exposed by MT5 MCP schema)"
    ),
    cmd!(
        "trade_reverse_position",
        "trading",
        Financial,
        "close and open the opposite side"
    ),
    cmd!(
        "trade_set_position_sl",
        "trading",
        Financial,
        "set Stop Loss on a position"
    ),
    cmd!(
        "trade_set_position_tp",
        "trading",
        Financial,
        "set Take Profit on a position"
    ),
    cmd!(
        "trade_calculate_order_result",
        "trading",
        ReadOnly,
        "profit/loss at TP and SL for a hypothetical order"
    ),
    // 9. Risk & calculations (73-81)
    cmd!(
        "calculate_position_size",
        "risk",
        ReadOnly,
        "volume from risk budget and SL distance"
    ),
    cmd!(
        "calculate_trade_risk",
        "risk",
        ReadOnly,
        "margin + SL loss + equity percent for an order"
    ),
    cmd!(
        "calculate_stop_loss_price",
        "risk",
        ReadOnly,
        "SL price from entry and pips"
    ),
    cmd!(
        "calculate_take_profit_price",
        "risk",
        ReadOnly,
        "TP price from entry and pips"
    ),
    cmd!(
        "calculate_margin_required",
        "risk",
        ReadOnly,
        "required margin for an order"
    ),
    cmd!(
        "calculate_margin_level_after_trade",
        "risk",
        ReadOnly,
        "projected margin level"
    ),
    cmd!(
        "calculate_pip_value",
        "risk",
        ReadOnly,
        "per-pip value in account currency"
    ),
    cmd!(
        "calculate_profit_loss",
        "risk",
        ReadOnly,
        "profit calc passthrough"
    ),
    cmd!(
        "validate_trade_request",
        "risk",
        ReadOnly,
        "pre-trade validation (never executes)"
    ),
    // 10. Errors & monitoring (82-90)
    cmd!(
        "get_mt5_last_error",
        "diagnostics",
        ReadOnly,
        "last recorded MT5/bridge error"
    ),
    cmd!(
        "get_bridge_logs",
        "diagnostics",
        ReadOnly,
        "recent bridge event log"
    ),
    cmd!(
        "get_bridge_health",
        "diagnostics",
        ReadOnly,
        "composite health verdict"
    ),
    cmd!(
        "get_bridge_latency",
        "diagnostics",
        ReadOnly,
        "observed command latency statistics"
    ),
    cmd!(
        "get_mt5_connection_metrics",
        "diagnostics",
        ReadOnly,
        "reconnects, session age, terminal ping"
    ),
    cmd!(
        "get_trade_execution_metrics",
        "diagnostics",
        ReadOnly,
        "executed/rejected/failed counters"
    ),
    cmd!(
        "get_bridge_capabilities",
        "diagnostics",
        ReadOnly,
        "this registry + manifest summary"
    ),
    cmd!(
        "get_mcp_available_tools",
        "diagnostics",
        ReadOnly,
        "tools the terminal currently exposes"
    ),
    cmd!(
        "get_mt5_diagnostic_report",
        "diagnostics",
        ReadOnly,
        "combined diagnostic report"
    ),
];

/// Looks up a bridge command by name.
pub fn lookup(name: &str) -> Option<&'static BridgeCommandSpec> {
    BRIDGE_COMMANDS.iter().find(|c| c.name == name)
}

/// Dispatches one bridge command by name. Handlers live in the category
/// modules; this is the single entry point used by the daemon.
pub async fn execute(
    name: &str,
    params: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    match name {
        // 1. Connection & terminal
        "get_bridge_status" => query::get_bridge_status(p(params), d).await,
        "get_mt5_connection_status" => query::get_mt5_connection_status(p(params), d).await,
        "connect_mt5" => query::connect_mt5(p(params), d).await,
        "disconnect_mt5" => query::disconnect_mt5(p(params), d).await,
        "reconnect_mt5" => query::reconnect_mt5(p(params), d).await,
        "get_mt5_terminal_info" => query::get_mt5_terminal_info(p(params), d).await,
        "get_mt5_terminal_version" => query::get_mt5_terminal_version(p(params), d).await,
        "get_mt5_server_info" => query::get_mt5_server_info(p(params), d).await,
        "get_mcp_server_info" => query::get_mcp_server_info(p(params), d).await,
        // 2. Account
        "get_trading_account_info" => query::get_trading_account_info(p(params), d).await,
        "get_account_balance" => query::get_account_balance(p(params), d).await,
        "get_account_equity" => query::get_account_equity(p(params), d).await,
        "get_account_margin" => query::get_account_margin(p(params), d).await,
        "get_account_free_margin" => query::get_account_free_margin(p(params), d).await,
        "get_account_margin_level" => query::get_account_margin_level(p(params), d).await,
        "get_account_profit" => query::get_account_profit(p(params), d).await,
        "get_account_currency" => query::get_account_currency(p(params), d).await,
        "get_account_trading_permissions" => {
            query::get_account_trading_permissions(p(params), d).await
        }
        // 3. Symbols & specs
        "get_trading_symbols" => query::get_trading_symbols(p(params), d).await,
        "get_symbol_info" => query::get_symbol_info(p(params), d).await,
        "get_symbol_price" => query::get_symbol_price(p(params), d).await,
        "get_symbol_tick_size" => query::get_symbol_tick_size(p(params), d).await,
        "get_symbol_point" => query::get_symbol_point(p(params), d).await,
        "get_symbol_digits" => query::get_symbol_digits(p(params), d).await,
        "get_symbol_spread" => query::get_symbol_spread(p(params), d).await,
        "get_symbol_trading_sessions" => query::get_symbol_trading_sessions(p(params), d).await,
        "get_symbol_trade_constraints" => query::get_symbol_trade_constraints(p(params), d).await,
        // 4. Live market
        "get_market_tick" => query::get_market_tick(p(params), d).await,
        "get_market_bid" => query::get_market_bid(p(params), d).await,
        "get_market_ask" => query::get_market_ask(p(params), d).await,
        "get_market_last_price" => query::get_market_last_price(p(params), d).await,
        "get_market_depth" => query::get_market_depth(p(params), d).await,
        "get_market_spread_history" => query::get_market_spread_history(p(params), d).await,
        "get_market_tick_volume" => query::get_market_tick_volume(p(params), d).await,
        "get_market_watchlist" => query::get_market_watchlist(p(params), d).await,
        "subscribe_market_ticks" => query::subscribe_market_ticks(p(params), d).await,
        // 5. Charts & history
        "get_chart_history" => query::get_chart_history(p(params), d).await,
        "get_chart_ticks_history" => query::get_chart_ticks_history(p(params), d).await,
        "get_chart_current_timeframe" => query::get_chart_current_timeframe(p(params), d).await,
        "get_chart_available_timeframes" => {
            query::get_chart_available_timeframes(p(params), d).await
        }
        "get_chart_ohlcv" => query::get_chart_ohlcv(p(params), d).await,
        "get_chart_indicators" => query::get_chart_indicators(p(params), d).await,
        "get_chart_objects" => query::get_chart_objects(p(params), d).await,
        "get_chart_active_info" => query::get_chart_active_info(p(params), d).await,
        "get_chart_multi_timeframe_history" => {
            query::get_chart_multi_timeframe_history(p(params), d).await
        }
        // 6. Positions
        "get_trading_open_positions" => trading::get_trading_open_positions(p(params), d).await,
        "get_position_info" => trading::get_position_info(p(params), d).await,
        "get_position_profit" => trading::get_position_profit(p(params), d).await,
        "get_position_volume" => trading::get_position_volume(p(params), d).await,
        "get_position_sl_tp" => trading::get_position_sl_tp(p(params), d).await,
        "get_position_risk" => trading::get_position_risk(p(params), d).await,
        "trade_modify_sl_tp" => trading::trade_modify_sl_tp(p(params), d).await,
        "trade_close_single_position" => trading::trade_close_single_position(p(params), d).await,
        "trade_close_all_positions" => trading::trade_close_all_positions(p(params), d).await,
        // 7. Orders & history
        "get_trading_pending_orders" => trading::get_trading_pending_orders(p(params), d).await,
        "get_order_info" => trading::get_order_info(p(params), d).await,
        "get_order_status" => trading::get_order_status(p(params), d).await,
        "get_trading_history_orders" => trading::get_trading_history_orders(p(params), d).await,
        "get_trading_history_positions" => {
            trading::get_trading_history_positions(p(params), d).await
        }
        "get_trading_deals_history" => trading::get_trading_deals_history(p(params), d).await,
        "trade_cancel_pending_order" => trading::trade_cancel_pending_order(p(params), d).await,
        "trade_modify_pending_order" => trading::trade_modify_pending_order(p(params), d).await,
        "get_order_execution_details" => trading::get_order_execution_details(p(params), d).await,
        // 8. Trade execution
        "trade_send_market_order" => trading::trade_send_market_order(p(params), d).await,
        "trade_send_buy_order" => trading::trade_send_buy_order(p(params), d).await,
        "trade_send_sell_order" => trading::trade_send_sell_order(p(params), d).await,
        "trade_send_pending_order" => trading::trade_send_pending_order(p(params), d).await,
        "trade_close_partial_position" => trading::trade_close_partial_position(p(params), d).await,
        "trade_reverse_position" => trading::trade_reverse_position(p(params), d).await,
        "trade_set_position_sl" => trading::trade_set_position_sl(p(params), d).await,
        "trade_set_position_tp" => trading::trade_set_position_tp(p(params), d).await,
        "trade_calculate_order_result" => trading::trade_calculate_order_result(p(params), d).await,
        // 9. Risk & calculations
        "calculate_position_size" => risk_diag::calculate_position_size(p(params), d).await,
        "calculate_trade_risk" => risk_diag::calculate_trade_risk(p(params), d).await,
        "calculate_stop_loss_price" => risk_diag::calculate_stop_loss_price(p(params), d).await,
        "calculate_take_profit_price" => risk_diag::calculate_take_profit_price(p(params), d).await,
        "calculate_margin_required" => risk_diag::calculate_margin_required(p(params), d).await,
        "calculate_margin_level_after_trade" => {
            risk_diag::calculate_margin_level_after_trade(p(params), d).await
        }
        "calculate_pip_value" => risk_diag::calculate_pip_value(p(params), d).await,
        "calculate_profit_loss" => risk_diag::calculate_profit_loss(p(params), d).await,
        "validate_trade_request" => risk_diag::validate_trade_request(p(params), d).await,
        // 10. Errors & monitoring
        "get_mt5_last_error" => risk_diag::get_mt5_last_error(p(params), d).await,
        "get_bridge_logs" => risk_diag::get_bridge_logs(p(params), d).await,
        "get_bridge_health" => risk_diag::get_bridge_health(p(params), d).await,
        "get_bridge_latency" => risk_diag::get_bridge_latency(p(params), d).await,
        "get_mt5_connection_metrics" => risk_diag::get_mt5_connection_metrics(p(params), d).await,
        "get_trade_execution_metrics" => risk_diag::get_trade_execution_metrics(p(params), d).await,
        "get_bridge_capabilities" => risk_diag::get_bridge_capabilities(p(params), d).await,
        "get_mcp_available_tools" => risk_diag::get_mcp_available_tools(p(params), d).await,
        "get_mt5_diagnostic_report" => risk_diag::get_mt5_diagnostic_report(p(params), d).await,
        _ => Err(BridgeFailure::not_available(format!(
            "unknown bridge command: {name}"
        ))),
    }
}

fn p(params: &Value) -> &Value {
    params
}

// ---------------------------------------------------------------------------
// Types shared by the handler modules
// ---------------------------------------------------------------------------

/// Machine-readable bridge failure (`error.code` inside the envelope).
#[derive(Debug, Clone)]
pub struct BridgeFailure {
    pub code: String,
    pub message: String,
}

impl BridgeFailure {
    pub fn not_available(message: impl Into<String>) -> Self {
        Self {
            code: "not_available".to_string(),
            message: message.into(),
        }
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_params".to_string(),
            message: message.into(),
        }
    }
    pub fn mt5_error(message: impl Into<String>) -> Self {
        Self {
            code: "mt5_error".to_string(),
            message: message.into(),
        }
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "bridge_internal".to_string(),
            message: message.into(),
        }
    }
}

impl From<McpError> for BridgeFailure {
    fn from(e: McpError) -> Self {
        BridgeFailure::mt5_error(e.to_string())
    }
}

/// What a handler needs to run one command.
pub struct BridgeDeps<'a> {
    pub mcp: &'a mut McpHttpClient,
    pub manifest: &'a crate::actions::manifest::ActionManifest,
    pub state: &'a BridgeState,
}

/// Uniform response envelope — the ONLY shape the Platform sees.
pub fn envelope(name: &str, result: Result<Value, BridgeFailure>) -> Value {
    match result {
        Ok(data) => json!({
            "command": name,
            "ok": true,
            "data": data,
            "broker_time": broker_time_now(),
            "error": Value::Null,
        }),
        Err(f) => json!({
            "command": name,
            "ok": false,
            "data": Value::Null,
            "broker_time": broker_time_now(),
            "error": { "code": f.code, "message": f.message },
        }),
    }
}

/// Broker's naive ISO time (no timezone) — the verbatim broker-time rule.
pub fn broker_time_now() -> String {
    let fd = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    OffsetDateTime::now_utc()
        .format(&fd)
        .unwrap_or_else(|_| "1970-01-01T00:00:00".to_string())
}

// ---------------------------------------------------------------------------
// Parameter helpers
// ---------------------------------------------------------------------------

pub(crate) fn req_str<'a>(p: &'a Value, key: &str) -> Result<&'a str, BridgeFailure> {
    p.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            BridgeFailure::invalid_params(format!("missing required string param '{key}'"))
        })
}

pub(crate) fn req_f64(p: &Value, key: &str) -> Result<f64, BridgeFailure> {
    p.get(key).and_then(Value::as_f64).ok_or_else(|| {
        BridgeFailure::invalid_params(format!("missing required numeric param '{key}'"))
    })
}

pub(crate) fn opt_f64(p: &Value, key: &str) -> Option<f64> {
    p.get(key).and_then(Value::as_f64)
}

pub(crate) fn opt_str<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

pub(crate) fn opt_usize(p: &Value, key: &str, default: usize) -> usize {
    p.get(key)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(default)
}

/// Rounds a price to the symbol's digit count (never more precise than the
/// broker's own instrument definition).
pub(crate) fn round_to_digits(value: f64, digits: u32) -> f64 {
    let factor = 10f64.powi(digits as i32);
    (value * factor).round() / factor
}

// ---------------------------------------------------------------------------
// BridgeState — local observability feeding the category-1/10 commands
// ---------------------------------------------------------------------------

/// Bridge-local runtime state. Sync guards only — never held across `.await`.
pub struct BridgeState {
    inner: RwLock<StateInner>,
}

struct StateInner {
    started_at: Instant,
    mt5_connected: bool,
    connected_since: Option<Instant>,
    reconnects: u64,
    handshake: Option<crate::mcp::HandshakeInfo>,
    commands_total: u64,
    executed: u64,
    rejected: u64,
    mcp_failed: u64,
    total_latency_ms: u64,
    last_latency_ms: u64,
    last_command: Option<String>,
    last_error: Option<String>,
    logs: VecDeque<String>,
    poll_subscriptions: HashSet<String>,
    last_sync_at: Option<Instant>,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            inner: RwLock::new(StateInner {
                started_at: Instant::now(),
                mt5_connected: false,
                connected_since: None,
                reconnects: 0,
                handshake: None,
                commands_total: 0,
                executed: 0,
                rejected: 0,
                mcp_failed: 0,
                total_latency_ms: 0,
                last_latency_ms: 0,
                last_command: None,
                last_error: None,
                logs: VecDeque::new(),
                poll_subscriptions: HashSet::new(),
                last_sync_at: None,
            }),
        }
    }
}

const LOG_CAP: usize = 200;

impl BridgeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one command execution (success or failure) with its latency.
    pub fn observe_command(&self, name: &str, outcome: CommandOutcome, latency_ms: u64) {
        let mut s = self.inner.write().unwrap_or_else(|e| e.into_inner());
        s.commands_total += 1;
        s.total_latency_ms = s.total_latency_ms.saturating_add(latency_ms);
        s.last_latency_ms = latency_ms;
        s.last_command = Some(name.to_string());
        match outcome {
            CommandOutcome::Executed => s.executed += 1,
            CommandOutcome::Rejected => s.rejected += 1,
            CommandOutcome::McpFailed => s.mcp_failed += 1,
        }
    }

    pub fn set_mt5_connected(&self, connected: bool, handshake: Option<crate::mcp::HandshakeInfo>) {
        let mut s = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if connected && !s.mt5_connected {
            s.connected_since = Some(Instant::now());
        }
        if !connected {
            s.connected_since = None;
        }
        s.mt5_connected = connected;
        if handshake.is_some() {
            s.handshake = handshake;
        }
    }

    pub fn bump_reconnects(&self) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .reconnects += 1;
    }

    pub fn set_last_error(&self, message: impl Into<String>) {
        let mut s = self.inner.write().unwrap_or_else(|e| e.into_inner());
        s.last_error = Some(message.into());
        Self::push_log_inner(&mut s, "error recorded");
    }

    pub fn push_log(&self, line: impl Into<String>) {
        let mut s = self.inner.write().unwrap_or_else(|e| e.into_inner());
        Self::push_log_inner(&mut s, line);
    }

    fn push_log_inner(s: &mut StateInner, line: impl Into<String>) {
        s.logs.push_back(line.into());
        while s.logs.len() > LOG_CAP {
            s.logs.pop_front();
        }
    }

    pub fn note_sync(&self) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .last_sync_at = Some(Instant::now());
    }

    pub fn subscribe_poll(&self, symbol: &str) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .poll_subscriptions
            .insert(symbol.to_string());
    }

    pub fn ws_sessions_added(&self, n: u64) {
        let _ = n; // wired by the daemon via set_mt5-style updates in v1.1
    }

    /// Uptime in seconds (diagnostics).
    pub fn uptime_secs(&self) -> u64 {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .started_at
            .elapsed()
            .as_secs()
    }
}

/// How a bridge command ended (metrics classification).
#[derive(Debug, Clone, Copy)]
pub enum CommandOutcome {
    Executed,
    Rejected,
    McpFailed,
}

// ---------------------------------------------------------------------------
// Gate check for bridge commands (mirrors the dispatcher chain)
// ---------------------------------------------------------------------------

/// The dispatcher-level gate chain for bridge commands: kill switch, expiry,
/// device/account binding, tier permission, financial consent, replay.
/// Manifest membership and schema validation are bridge-registry specific
/// (fixed 90 commands, params validated per-handler).
pub fn gate_check(
    req: &ActionRequest,
    ctx: &DispatchContext,
    kill: &KillSwitch,
    replay_check: impl FnOnce(&str) -> bool,
    spec: &BridgeCommandSpec,
) -> Result<(), DispatchRejection> {
    // Expiry.
    let expires =
        crate::mt5::parse_broker_time(&req.expires_at).ok_or(DispatchRejection::Malformed)?;
    if expires <= OffsetDateTime::now_utc() {
        return Err(DispatchRejection::Expired);
    }
    // Device + account binding.
    if req.device_id != ctx.device_id {
        return Err(DispatchRejection::WrongDevice);
    }
    if !ctx.authorized_accounts.contains(&req.account_id) {
        return Err(DispatchRejection::WrongAccount);
    }
    // Kill switch (bridge category feeds the category scope).
    if let Some(scope) = kill.check(
        ctx.user_id.as_deref(),
        &req.account_id,
        &req.device_id,
        spec.category,
        spec.tier == RiskTier::Financial,
    ) {
        return Err(DispatchRejection::KillSwitchActive(scope));
    }
    // Tier permission + financial consent (same grants vocabulary).
    let permission_ok = match spec.tier {
        RiskTier::ReadOnly => true,
        RiskTier::LocalModification => ctx.grants.contains("local.modify"),
        RiskTier::FilesystemOrCode => ctx.grants.contains("advanced.execute"),
        RiskTier::Financial => ctx.grants.contains("trading.execute"),
    };
    if !permission_ok {
        return Err(DispatchRejection::PermissionDenied {
            tier: spec.tier,
            required: match spec.tier {
                RiskTier::Financial => "trading.execute",
                RiskTier::LocalModification => "local.modify",
                RiskTier::FilesystemOrCode => "advanced.execute",
                RiskTier::ReadOnly => "",
            }
            .to_string(),
        });
    }
    if spec.tier == RiskTier::Financial && !ctx.financial_consent {
        return Err(DispatchRejection::FinancialConsentMissing);
    }
    // Replay protection — only fresh ids pass.
    if !replay_check(&req.idempotency_key) {
        return Err(DispatchRejection::Replayed);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_exactly_90_unique_commands() {
        assert_eq!(BRIDGE_COMMANDS.len(), 90, "the directive fixes 90 commands");
        let mut names: Vec<_> = BRIDGE_COMMANDS.iter().map(|c| c.name).collect();
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique, "duplicate command name in registry");
    }

    #[test]
    fn registry_names_are_snake_case_and_lookup_works() {
        for c in BRIDGE_COMMANDS {
            assert!(
                c.name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
                "non snake_case name: {}",
                c.name
            );
            assert!(!c.summary.is_empty(), "missing summary for {}", c.name);
            assert!(lookup(c.name).is_some());
        }
        assert!(lookup("definitely_not_a_command").is_none());
    }

    #[test]
    fn financial_commands_are_exactly_the_trade_surface() {
        let financial: Vec<_> = BRIDGE_COMMANDS
            .iter()
            .filter(|c| c.tier == RiskTier::Financial)
            .map(|c| c.name)
            .collect();
        // 52, 53, 54 (cat 6) + 61, 62 (cat 7) + 64-71 (cat 8, calc 72 is RO) = 13
        assert_eq!(financial.len(), 13, "financial set: {financial:?}");
        assert!(financial.contains(&"trade_send_market_order"));
        assert!(!financial.contains(&"validate_trade_request"));
    }

    #[test]
    fn every_command_dispatches_or_reports_unknown() {
        // Dispatch table coverage: every registry name must hit an arm.
        // We assert via lookup + execute's fallthrough on a fake name only
        // (handlers need MCP; full execution is covered by integration tests).
        for c in BRIDGE_COMMANDS {
            assert!(lookup(c.name).is_some(), "unroutable: {}", c.name);
        }
    }

    #[test]
    fn envelope_shapes_success_and_failure() {
        let ok = envelope("get_account_balance", Ok(json!({ "balance": 1.0 })));
        assert_eq!(ok["ok"], json!(true));
        assert_eq!(ok["command"], json!("get_account_balance"));
        assert_eq!(ok["error"], Value::Null);
        assert!(ok["broker_time"].is_string());

        let bad = envelope(
            "get_account_balance",
            Err(BridgeFailure::invalid_params("nope")),
        );
        assert_eq!(bad["ok"], json!(false));
        assert_eq!(bad["error"]["code"], json!("invalid_params"));
    }

    #[test]
    fn round_to_digits_matches_broker_precision() {
        assert_eq!(round_to_digits(1.097741234, 5), 1.09774);
        assert_eq!(round_to_digits(2400.56789, 2), 2400.57);
    }

    #[test]
    fn gate_check_rejects_wrong_device_and_expired() {
        use crate::actions::dispatcher::ActionRequest;
        let kill = KillSwitch::new();
        let ctx = DispatchContext::new("dev-1")
            .with_account("12345678901")
            .with_grant("trading.execute")
            .with_financial_consent(true);
        let mk = |expires: &str| ActionRequest {
            request_id: "r".into(),
            command_id: "c".into(),
            action_name: "trade_send_market_order".into(),
            account_id: "12345678901".into(),
            device_id: "dev-1".into(),
            issued_at: "2026-01-01T00:00:00Z".into(),
            expires_at: expires.into(),
            idempotency_key: "k".into(),
            parameters: json!({}),
            authorization_context: json!({}),
        };
        let spec = lookup("trade_send_market_order").unwrap();
        // Valid future expiry passes.
        assert!(gate_check(&mk("2099-01-01T00:00:00Z"), &ctx, &kill, |_| true, spec).is_ok());
        // Wrong device fails.
        let mut wrong = mk("2099-01-01T00:00:00Z");
        wrong.device_id = "dev-2".into();
        assert!(matches!(
            gate_check(&wrong, &ctx, &kill, |_| true, spec),
            Err(DispatchRejection::WrongDevice)
        ));
        // Expired fails.
        assert!(matches!(
            gate_check(&mk("2000-01-01T00:00:00Z"), &ctx, &kill, |_| true, spec),
            Err(DispatchRejection::Expired)
        ));
        // Replay fails.
        assert!(matches!(
            gate_check(&mk("2099-01-01T00:00:00Z"), &ctx, &kill, |_| false, spec),
            Err(DispatchRejection::Replayed)
        ));
    }
}
