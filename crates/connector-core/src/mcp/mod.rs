//! MCP (Model Context Protocol) streamable-HTTP client for the local
//! MetaTrader 5 MCP server.
//!
//! Wire behaviors verified against a live `MetaTrader 5 MCP` v1.0 server
//! (2025-06-18 protocol):
//! - Bearer auth is mandatory: missing/invalid token => HTTP 401 with
//!   `WWW-Authenticate: Bearer`.
//! - A session id is issued in the `Mcp-Session-Id` response header on
//!   `initialize` and must be echoed on every later request; an unknown
//!   session id yields HTTP 404 with an empty body.
//! - `notifications/initialized` answers HTTP 202.
//! - An unsupported `protocolVersion` in `initialize` is *negotiated down*:
//!   the server answers with its own version, so the client must verify the
//!   negotiated value instead of assuming acceptance.
//! - Unknown tools return HTTP 200 with a JSON-RPC error `-32602
//!   "tool not found"` (protocol-level error, not an HTTP-level one).
//! - Tool results carry the payload as a JSON string inside
//!   `result.content[0].text` (JSON-in-text), with `result.isError: false`.

mod client;
mod error;

pub use client::{
    HandshakeInfo, McpClientConfig, McpHttpClient, SUPPORTED_PROTOCOL_VERSION, ToolDescriptor,
    ToolOutcome, extract_tool_json, extract_tool_result,
};
pub use error::McpError;

/// Tool names the Connector requires for the data pipeline. Discovery is
/// dynamic; these are the minimum capabilities the bridge depends on.
pub mod trading_tools {
    pub const ACCOUNT_INFO: &str = "get_trading_account_info";
    pub const OPEN_POSITIONS: &str = "get_trading_open_positions";
    pub const HISTORY_ORDERS: &str = "get_trading_history_orders";
    pub const HISTORY_POSITIONS: &str = "get_trading_history_positions";

    /// The minimum set the Connector needs before starting synchronization.
    pub const REQUIRED: [&str; 4] = [
        ACCOUNT_INFO,
        OPEN_POSITIONS,
        HISTORY_ORDERS,
        HISTORY_POSITIONS,
    ];
}
