//! Mapping from extracted tool JSON payloads to typed MT5 models.

use serde_json::Value;

use crate::mcp::McpError;

use super::models::{
    AccountSnapshot, HistoryOrdersResponse, HistoryPositionsResponse, OpenPositionsResponse,
};

fn map<T: serde::de::DeserializeOwned>(v: &Value, what: &str) -> Result<T, McpError> {
    serde_json::from_value(v.clone()).map_err(|e| McpError::Malformed(format!("{what}: {e}")))
}

/// Maps the extracted `get_trading_account_info` payload to an
/// [`AccountSnapshot`].
pub fn account_snapshot(v: &Value) -> Result<AccountSnapshot, McpError> {
    map(v, "account info payload")
}

/// Maps the extracted `get_trading_open_positions` payload.
pub fn open_positions(v: &Value) -> Result<OpenPositionsResponse, McpError> {
    map(v, "open positions payload")
}

/// Maps the extracted `get_trading_history_orders` payload.
pub fn history_orders(v: &Value) -> Result<HistoryOrdersResponse, McpError> {
    map(v, "history orders payload")
}

/// Maps the extracted `get_trading_history_positions` payload.
pub fn history_positions(v: &Value) -> Result<HistoryPositionsResponse, McpError> {
    map(v, "history positions payload")
}
