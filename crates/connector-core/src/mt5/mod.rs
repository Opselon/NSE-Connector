//! MT5 data models and mapping from MCP tool-call payloads.

mod mapping;
mod models;

pub use mapping::{account_snapshot, history_orders, history_positions, open_positions};
pub use models::{
    AccountInfo, AccountSnapshot, HistoryOrdersResponse, HistoryPositionsResponse, Mt5Deal,
    Mt5HistoryPosition, Mt5Order, Mt5Position, OpenPositionsResponse, TerminalConnection,
    TerminalInfo, flex_string, parse_broker_time,
};
