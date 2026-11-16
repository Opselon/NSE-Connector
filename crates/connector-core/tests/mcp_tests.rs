//! MCP tests (engineering spec §16 "MCP Tests") — every bullet mapped to a
//! named test, executed against the mock MCP server with injected failure
//! scenarios. Live-server equivalents live in `live_mt5_tests.rs`.

mod common;

use std::time::{Duration, Instant};

use common::mock_mcp::{MockMcp, ToolBehavior};
use common::{account_payload, positions_payload};
use connector_core::mcp::{McpClientConfig, McpError, McpHttpClient};
use connector_core::mt5;
use connector_core::security::secrets::Secret;
use serde_json::json;

fn client(endpoint: &str, token: Option<&str>, timeout: Duration) -> McpHttpClient {
    McpHttpClient::new(McpClientConfig {
        endpoint: endpoint.to_string(),
        bearer: token.map(Secret::new),
        request_timeout: timeout,
        connect_timeout: timeout,
        ..Default::default()
    })
    .expect("client configuration is valid")
}

// ---------------------------------------------------------------------------
// Valid endpoint / MCP initialization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn valid_endpoint_connects_and_initializes() {
    let mock = MockMcp::spawn().await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    let info = c.connect().await.expect("handshake must succeed");
    assert_eq!(
        info.protocol_version,
        connector_core::mcp::SUPPORTED_PROTOCOL_VERSION
    );
    assert_eq!(info.server_name, "MockMT5-MCP");
    assert!(info.session_id.is_some(), "server must issue a session id");
    assert_eq!(c.session_id(), info.session_id.as_deref());
    mock.stop().await;
}

#[tokio::test]
async fn reconnect_creates_a_fresh_session() {
    let mock = MockMcp::spawn().await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    let first = c.connect().await.expect("first handshake");
    let second = c.connect().await.expect("second handshake");
    assert_ne!(
        first.session_id, second.session_id,
        "each initialize issues a new session"
    );
    assert_eq!(mock.session_count().await, 2);
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Invalid endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_endpoint_fails_fast_with_transport_error() {
    // Take a real port, then stop the server so the address is guaranteed
    // closed (no firewall-induced hangs on customer machines).
    let mock = MockMcp::spawn().await;
    let endpoint = mock.endpoint();
    mock.stop().await;

    let started = Instant::now();
    let mut c = client(&endpoint, Some("test-token"), Duration::from_secs(5));
    let err = c.connect().await.expect_err("closed endpoint must fail");
    assert!(matches!(err, McpError::Transport(_)), "got: {err:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "must fail fast");
}

// ---------------------------------------------------------------------------
// MCP authentication failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_token_rejected_with_unauthorized() {
    let mock = MockMcp::spawn().await;
    let mut c = client(&mock.endpoint(), None, Duration::from_secs(5));
    let err = c.connect().await.expect_err("missing token must fail");
    assert!(matches!(err, McpError::Unauthorized), "got: {err:?}");
    mock.stop().await;
}

#[tokio::test]
async fn wrong_token_rejected_with_unauthorized() {
    let mock = MockMcp::spawn().await;
    let mut c = client(
        &mock.endpoint(),
        Some("wrong-token"),
        Duration::from_secs(5),
    );
    let err = c.connect().await.expect_err("wrong token must fail");
    assert!(matches!(err, McpError::Unauthorized), "got: {err:?}");
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Unsupported protocol version (live servers negotiate down — client must
// verify the negotiated value)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn protocol_version_mismatch_is_detected() {
    let mock = MockMcp::spawn().await;
    mock.set_protocol_version("2099-01-01").await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    let err = c.connect().await.expect_err("unknown protocol must fail");
    assert!(
        matches!(
            err,
            McpError::ProtocolMismatch { ref negotiated, .. } if negotiated == "2099-01-01"
        ),
        "got: {err:?}"
    );
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Tool discovery / missing tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_discovery_returns_descriptors() {
    let mock = MockMcp::spawn().await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    c.connect().await.expect("handshake");
    let tools = c.list_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    for required in connector_core::mcp::trading_tools::REQUIRED {
        assert!(names.contains(&required), "missing tool {required}");
        assert!(
            tools
                .iter()
                .find(|t| t.name == required)
                .unwrap()
                .input_schema
                .is_some(),
            "descriptor should carry its input schema"
        );
    }
    mock.stop().await;
}

#[tokio::test]
async fn required_trading_tools_are_verified() {
    let mock = MockMcp::spawn().await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    c.connect().await.expect("handshake");
    let tools = c.list_tools().await.expect("tools/list");
    McpHttpClient::verify_required_tools(&tools, &connector_core::mcp::trading_tools::REQUIRED)
        .expect("all four trading tools present");
    mock.stop().await;
}

#[tokio::test]
async fn missing_required_tools_are_detected() {
    let mock = MockMcp::spawn().await;
    // Server without the account-info tool (e.g. a restricted installation).
    mock.set_tools(json!([
        { "name": connector_core::mcp::trading_tools::OPEN_POSITIONS }
    ]))
    .await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    c.connect().await.expect("handshake");
    let tools = c.list_tools().await.expect("tools/list");
    let err =
        McpHttpClient::verify_required_tools(&tools, &connector_core::mcp::trading_tools::REQUIRED)
            .expect_err("capability check must fail");
    assert!(
        matches!(err, McpError::MissingTools(ref m) if m.contains(&connector_core::mcp::trading_tools::ACCOUNT_INFO.to_string())),
        "got: {err:?}"
    );
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Malformed responses / JSON-in-text extraction / tool errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_in_text_envelope_is_extracted() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::TextJson(account_payload()),
    )
    .await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    let payload = c
        .call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect("JSON-in-text must be extracted");
    assert_eq!(payload.pointer("/account/balance"), Some(&json!(26237.73)));
    mock.stop().await;
}

#[tokio::test]
async fn tool_error_flag_maps_to_tool_failed() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::IsError("terminal not running".to_string()),
    )
    .await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    let err = c
        .call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect_err("isError must surface");
    assert!(
        matches!(err, McpError::ToolFailed(ref m) if m.contains("terminal not running")),
        "got: {err:?}"
    );
    mock.stop().await;
}

#[tokio::test]
async fn unknown_tool_maps_to_jsonrpc_tool_not_found() {
    let mock = MockMcp::spawn().await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    c.connect().await.expect("handshake");
    let err = c
        .call_tool_json("totally_unknown_tool", &json!({}))
        .await
        .expect_err("unknown tool must fail");
    assert!(matches!(err, McpError::ToolNotFound(name) if name == "totally_unknown_tool"));
    mock.stop().await;
}

#[tokio::test]
async fn malformed_json_body_maps_to_malformed() {
    let mock = MockMcp::spawn().await;
    mock.set_malformed_json(true).await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    let err = c.connect().await.expect_err("garbage body must fail");
    assert!(matches!(err, McpError::Malformed(_)), "got: {err:?}");
    mock.stop().await;
}

#[tokio::test]
async fn malformed_tool_text_maps_to_malformed() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::TextNotJson("<html>not json</html>"),
    )
    .await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    let err = c
        .call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect_err("non-JSON text must fail");
    assert!(matches!(err, McpError::Malformed(_)), "got: {err:?}");
    mock.stop().await;
}

#[tokio::test]
async fn result_without_content_maps_to_malformed() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::RawResult(json!({ "unexpected": true })),
    )
    .await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    let err = c
        .call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect_err("missing content array must fail");
    assert!(matches!(err, McpError::Malformed(_)), "got: {err:?}");
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Partial account information
// ---------------------------------------------------------------------------

#[tokio::test]
async fn partial_account_information_is_tolerated() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::TextJson(json!({ "account": { "login": "12345678901" } })),
    )
    .await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    let payload = c
        .call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect("partial payload must still parse");
    let snap = mt5::account_snapshot(&payload).expect("mapping");
    assert_eq!(snap.account.login.as_deref(), Some("12345678901"));
    assert_eq!(snap.account.balance, None, "absent fields stay None");
    assert!(snap.terminal.is_none());
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_timeout_is_enforced() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::Hang(Duration::from_millis(700)),
    )
    .await;
    let mut c = client(
        &mock.endpoint(),
        Some("test-token"),
        Duration::from_millis(150),
    );
    c.connect().await.expect("handshake");
    let started = Instant::now();
    let err = c
        .call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect_err("hung server must time out");
    assert!(
        matches!(err, McpError::Timeout(d) if d == Duration::from_millis(150)),
        "got: {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "timeout must not wait for server"
    );
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Server restart (session invalidation => 404 => transparent re-initialize)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_restart_triggers_single_reinit_and_recovery() {
    let mock = MockMcp::spawn().await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    c.connect().await.expect("initial handshake");
    let first = c
        .call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect("first call");

    // Simulate a server restart: every known session is gone (live server
    // answers 404 for unknown sessions).
    mock.invalidate_sessions().await;
    let sessions_before = mock.session_count().await;

    // High-level call recovers transparently.
    let second = c
        .call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect("call after restart must re-initialize once and succeed");
    assert_eq!(
        first.pointer("/account/balance"),
        second.pointer("/account/balance")
    );
    assert_eq!(
        mock.session_count().await,
        sessions_before + 1,
        "exactly one re-init"
    );

    // And the strict (no-recovery) path exposes the raw 404 behavior.
    mock.invalidate_sessions().await;
    let err = c
        .call_tool_raw_once(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect_err("strict call must surface the session loss");
    assert!(matches!(err, McpError::SessionExpired), "got: {err:?}");
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Mapping sanity on the verified live shape (empty account state)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_account_state_maps_cleanly() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::OPEN_POSITIONS,
        ToolBehavior::TextJson(json!({ "positions": [], "orders": [] })),
    )
    .await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::HISTORY_ORDERS,
        ToolBehavior::TextJson(json!({ "orders": [], "deals": [] })),
    )
    .await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::HISTORY_POSITIONS,
        ToolBehavior::TextJson(json!({ "positions": [] })),
    )
    .await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));

    let pos = mt5::open_positions(
        &c.call_tool_json(
            connector_core::mcp::trading_tools::OPEN_POSITIONS,
            &json!({}),
        )
        .await
        .expect("empty positions"),
    )
    .expect("mapping");
    assert!(pos.positions.is_empty() && pos.orders.is_empty());

    let hist = mt5::history_orders(
        &c.call_tool_json(
            connector_core::mcp::trading_tools::HISTORY_ORDERS,
            &json!({}),
        )
        .await
        .expect("empty history"),
    )
    .expect("mapping");
    assert!(hist.orders.is_empty() && hist.deals.is_empty());

    let hp = mt5::history_positions(
        &c.call_tool_json(
            connector_core::mcp::trading_tools::HISTORY_POSITIONS,
            &json!({}),
        )
        .await
        .expect("empty history positions"),
    )
    .expect("mapping");
    assert!(hp.positions.is_empty());
    mock.stop().await;
}

#[tokio::test]
async fn populated_positions_payload_maps_with_live_field_names() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::OPEN_POSITIONS,
        ToolBehavior::TextJson(positions_payload()),
    )
    .await;
    let mut c = client(&mock.endpoint(), Some("test-token"), Duration::from_secs(5));
    let payload = c
        .call_tool_json(
            connector_core::mcp::trading_tools::OPEN_POSITIONS,
            &json!({}),
        )
        .await
        .expect("positions payload");
    let pos = mt5::open_positions(&payload).expect("mapping");
    assert_eq!(pos.positions.len(), 2);
    assert_eq!(pos.orders.len(), 1);
    // Live-verified field names (IDs sanitized in the fixture).
    assert_eq!(
        pos.positions[0].position_id.as_deref(),
        Some("152913954001")
    );
    assert_eq!(pos.positions[0].action.as_deref(), Some("buy"));
    assert_eq!(pos.positions[0].price_last, Some(1.09951));
    assert_eq!(
        pos.positions[0].create_time.as_deref(),
        Some("2026-11-16T22:33:39")
    );
    assert_eq!(pos.orders[0].order_id.as_deref(), Some("152913954010"));
    assert!(pos.positions.iter().all(|p| p.is_reconcilable()));
    mock.stop().await;
}
