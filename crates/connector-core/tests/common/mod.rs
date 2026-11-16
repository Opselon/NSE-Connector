//! Shared test harness: mock MCP server (with injectable failure scenarios)
//! and mock NSE server (REST + WebSocket, recording all requests).
//!
//! Each integration-test binary includes this module but uses only part of
//! it; per-binary dead-code analysis therefore flags unused helpers, which
//! is expected for a shared harness.
#![allow(dead_code)]

pub mod mock_mcp;
pub mod mock_nse;

/// A representative MT5 account payload matching the LIVE verified shape
/// (fields verified against `get_trading_account_info` on 2026-11-16).
pub fn account_payload() -> serde_json::Value {
    serde_json::json!({
        "account": {
            "server": "Mock-Demo",
            "broker": "Mock Broker Ltd.",
            "login": "12345678901",
            "name": "Test User",
            "type": "demo",
            "read_only": false,
            "margin_mode": "hedging",
            "balance": 26237.73,
            "credit": 0.0,
            "margin": 0.0,
            "margin_free": 26237.73,
            "profit": 0.0,
            "equity": 26237.73,
            "swaps": 0.0,
            "commissions": 0.0,
            "currency": "USD"
        },
        "terminal": {
            "timestamp": "2026-11-16T20:31:21Z",
            "connection": {
                "state": "synchronized",
                "server": "Mock-Demo",
                "ping_microseconds": 103456
            },
            "build": 6306
        }
    })
}

/// Open-positions payload using the LIVE-verified field names
/// (`position_id`, `action`, `create_time`, `price_last`, `contract_size`,
/// `reason`; timestamps WITHOUT timezone suffix). IDs are sanitized.
pub fn positions_payload() -> serde_json::Value {
    serde_json::json!({
        "positions": [
            {
                "position_id": "152913954001",
                "action": "buy",
                "reason": "Client",
                "symbol": "EURUSD",
                "create_time": "2026-11-16T22:33:39",
                "update_time": "2026-11-16T22:34:10",
                "volume": 0.01,
                "price_open": 1.09995,
                "price_last": 1.09951,
                "profit": -0.44,
                "contract_size": 100000.0,
                "comment": ""
            },
            {
                "position_id": "152913954002",
                "action": "sell",
                "reason": "Client",
                "symbol": "XAUUSD",
                "create_time": "2026-11-16T23:02:31",
                "update_time": "2026-11-16T23:03:05",
                "volume": 0.05,
                "price_open": 2450.10,
                "price_last": 2448.65,
                "sl": 2452.0,
                "tp": 2444.0,
                "profit": 7.25,
                "swap": -0.12,
                "commission": -0.05,
                "contract_size": 100.0,
                "comment": ""
            }
        ],
        "orders": [
            {
                "order_id": "152913954010",
                "position_id": "",
                "symbol": "GBPUSD",
                "type": "buy limit",
                "filling": "return",
                "state": "placed",
                "open_time": "2026-11-16T11:00:00",
                "volume_initial": 0.2,
                "volume_current": 0.2,
                "contract_size": 100000.0,
                "price_open": 1.27,
                "sl": 1.2685,
                "tp": 1.274
            }
        ]
    })
}

/// History payload using the LIVE-verified field names (`order_id`,
/// `deal_id`, `entry`, `open_time`, `price_open`, `volume_initial` /
/// `volume_current`; no timezone suffix). IDs are sanitized.
pub fn history_payload() -> serde_json::Value {
    serde_json::json!({
        "orders": [
            {
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
            }
        ],
        "deals": [
            {
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
            },
            {
                "deal_id": "152756177828",
                "deal_external_id": "",
                "order_id": "152913954048",
                "position_id": "152913954047",
                "symbol": "EURUSD",
                "action": "sell",
                "entry": "out",
                "reason": "Client",
                "open_time": "2026-11-16T23:30:00",
                "price_open": 1.09951,
                "volume": 0.01,
                "profit": 4.40,
                "contract_size": 100000.0,
                "comment": ""
            }
        ]
    })
}

/// Same records expressed with ALTERNATIVE field names (the tolerant alias
/// layer must accept other MT5 MCP implementations' naming).
pub fn history_payload_alt_names() -> serde_json::Value {
    serde_json::json!({
        "orders": [
            { "ticket": "162000000001", "symbol": "USDJPY", "state": "filled" }
        ],
        "deals": [
            {
                "ticket": "162000000002",
                "symbol": "USDJPY",
                "direction": "sell",
                "entry": "out",
                "time": "2026-11-16T10:00:00Z",
                "price": 155.5,
                "profit": 12.0,
                "commission": -0.06
            }
        ]
    })
}
