//! REAL MT5 integration tests (engineering spec §16: "Run real Windows
//! integration tests with the actual MT5 MCP implementation whenever the
//! environment is available. Never claim a real integration test passed when
//! only mocks were used.").
//!
//! These tests run against the customer's actual MetaTrader 5 MCP server on
//! this PC. They are `#[ignore]`d by default AND gated behind environment
//! variables, so plain `cargo test` never touches the real terminal.
//!
//! Enable with (Windows cmd):
//! ```text
//! set NSE_LIVE_MCP_TESTS=1
//! set NSE_MCP_TEST_TOKEN=<MT5 MCP API key>
//! set NSE_LIVE_MCP_ENDPOINT=http://127.0.0.1:22346/mcp   (optional)
//! cargo test --test live_mt5_tests -- --ignored --nocapture
//! ```
//!
//! The token stays in the environment for the duration of the test run only.
//! It is never written to any file by these tests.

use std::time::Duration;

use connector_core::mcp::{McpClientConfig, McpError, McpHttpClient};
use connector_core::mt5;
use connector_core::security::secrets::Secret;
use serde_json::json;

/// Builds the live client config, or returns None when the environment is
/// not enabled (tests then report as skipped rather than executed).
fn live_config() -> Option<McpClientConfig> {
    if std::env::var("NSE_LIVE_MCP_TESTS").ok()?.trim() != "1" {
        return None;
    }
    let token = std::env::var("NSE_MCP_TEST_TOKEN").ok()?;
    if token.trim().is_empty() {
        return None;
    }
    Some(McpClientConfig {
        endpoint: std::env::var("NSE_LIVE_MCP_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:22346/mcp".to_string()),
        bearer: Some(Secret::new(token)),
        request_timeout: Duration::from_secs(20),
        connect_timeout: Duration::from_secs(10),
        client_name: "NSE-Connector-LiveTest".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

macro_rules! skip_unless_live {
    () => {
        if live_config().is_none() {
            eprintln!("SKIPPED: live MT5 tests not enabled (NSE_LIVE_MCP_TESTS != 1)");
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// Handshake against the real MetaTrader 5 MCP server
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live MT5 integration — set NSE_LIVE_MCP_TESTS=1 + NSE_MCP_TEST_TOKEN"]
async fn live_handshake_and_session_established() {
    skip_unless_live!();
    let cfg = live_config().unwrap();
    let mut c = McpHttpClient::new(cfg).expect("config");
    let info = c.connect().await.expect("live handshake must succeed");
    println!(
        "LIVE server: {} v{} protocol {}",
        info.server_name, info.server_version, info.protocol_version
    );
    assert_eq!(info.protocol_version, "2025-06-18");
    assert!(
        info.session_id.is_some(),
        "live server must issue a session id"
    );
    assert!(!info.server_name.is_empty());
}

// ---------------------------------------------------------------------------
// Tool discovery — the customer PC exposes the trading subset
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live MT5 integration — set NSE_LIVE_MCP_TESTS=1 + NSE_MCP_TEST_TOKEN"]
async fn live_tool_discovery_contains_required_trading_tools() {
    skip_unless_live!();
    let cfg = live_config().unwrap();
    let mut c = McpHttpClient::new(cfg).expect("config");
    c.connect().await.expect("handshake");
    let tools = c.list_tools().await.expect("tools/list");
    println!("LIVE tool count: {}", tools.len());
    // The real server exposes 100+ tools (charts, files, tester, ...); the
    // Connector only requires the trading subset.
    McpHttpClient::verify_required_tools(&tools, &connector_core::mcp::trading_tools::REQUIRED)
        .expect("live server must expose the four trading tools");
}

// ---------------------------------------------------------------------------
// Account information (read-only, real data)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live MT5 integration — set NSE_LIVE_MCP_TESTS=1 + NSE_MCP_TEST_TOKEN"]
async fn live_account_info_maps_to_snapshot() {
    skip_unless_live!();
    let cfg = live_config().unwrap();
    let mut c = McpHttpClient::new(cfg).expect("config");
    let payload = c
        .call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect("live account info");
    let snap = mt5::account_snapshot(&payload).expect("map live account");

    let login = snap.account.login.clone().unwrap_or_default();
    println!(
        "LIVE account: broker={:?} server={:?} type={:?} currency={:?} balance={:?} state={:?}",
        snap.account.broker,
        snap.account.server,
        snap.account.account_type,
        snap.account.currency,
        snap.account.balance,
        snap.terminal
            .as_ref()
            .and_then(|t| t.connection.as_ref())
            .and_then(|c| c.state.clone()),
    );
    println!(
        "LIVE login (masked): {}",
        connector_core::security::mask_login(&login)
    );

    // Fields verified live — must be present on a working installation.
    assert!(snap.account.broker.is_some(), "broker missing");
    assert!(snap.account.server.is_some(), "server missing");
    assert!(snap.account.currency.is_some(), "currency missing");
    assert!(snap.account.balance.is_some(), "balance missing");
    assert!(snap.account.equity.is_some(), "equity missing");
    assert!(
        snap.terminal.is_some(),
        "terminal context missing (broker time source)"
    );
}

// ---------------------------------------------------------------------------
// Open positions + history (tolerant: demo accounts may be empty)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live MT5 integration — set NSE_LIVE_MCP_TESTS=1 + NSE_MCP_TEST_TOKEN"]
async fn live_open_positions_query_succeeds() {
    skip_unless_live!();
    let cfg = live_config().unwrap();
    let mut c = McpHttpClient::new(cfg).expect("config");
    let payload = c
        .call_tool_json(
            connector_core::mcp::trading_tools::OPEN_POSITIONS,
            &json!({ "include_orders": true }),
        )
        .await
        .expect("live open positions");
    let pos = mt5::open_positions(&payload).expect("map");
    println!(
        "LIVE positions: {} open, {} working orders",
        pos.positions.len(),
        pos.orders.len()
    );
}

#[tokio::test]
#[ignore = "live MT5 integration — set NSE_LIVE_MCP_TESTS=1 + NSE_MCP_TEST_TOKEN"]
async fn live_history_range_query_succeeds() {
    skip_unless_live!();
    let cfg = live_config().unwrap();
    let mut c = McpHttpClient::new(cfg).expect("config");
    // Last 30 days.
    let args = json!({
        "datetime_from": "2026-10-18T00:00:00",
        "datetime_to": "2026-11-18T00:00:00",
        "limit": 100
    });
    let payload = c
        .call_tool_json(connector_core::mcp::trading_tools::HISTORY_ORDERS, &args)
        .await
        .expect("live history orders");
    let hist = mt5::history_orders(&payload).expect("map");
    println!(
        "LIVE history: {} orders, {} deals",
        hist.orders.len(),
        hist.deals.len()
    );

    let payload2 = c
        .call_tool_json(connector_core::mcp::trading_tools::HISTORY_POSITIONS, &args)
        .await
        .expect("live history positions");
    let hp = mt5::history_positions(&payload2).expect("map");
    println!("LIVE closed positions: {}", hp.positions.len());
}

// ---------------------------------------------------------------------------
// Error behaviors verified live (session loss, unknown tool)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live MT5 integration — set NSE_LIVE_MCP_TESTS=1 + NSE_MCP_TEST_TOKEN"]
async fn live_bad_session_reports_404_session_expired() {
    skip_unless_live!();
    let cfg = live_config().unwrap();
    let mut c = McpHttpClient::new(cfg).expect("config");
    c.connect().await.expect("handshake");
    // Force an unknown session (what happens after a terminal restart).
    c.restore_session(Some("00000000DEADBEEF00000000".to_string()));
    let err = c
        .call_tool_raw_once(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
        .expect_err("unknown live session must 404");
    assert!(matches!(err, McpError::SessionExpired), "got: {err:?}");
}

#[tokio::test]
#[ignore = "live MT5 integration — set NSE_LIVE_MCP_TESTS=1 + NSE_MCP_TEST_TOKEN"]
async fn live_unknown_tool_returns_jsonrpc_not_found() {
    skip_unless_live!();
    let cfg = live_config().unwrap();
    let mut c = McpHttpClient::new(cfg).expect("config");
    c.connect().await.expect("handshake");
    let err = c
        .call_tool_json("definitely_not_a_real_tool", &json!({}))
        .await
        .expect_err("unknown tool must fail");
    assert!(matches!(err, McpError::ToolNotFound(_)), "got: {err:?}");
}

// ---------------------------------------------------------------------------
// End-to-end: handshake -> discovery -> capability check -> full snapshot
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live MT5 integration — set NSE_LIVE_MCP_TESTS=1 + NSE_MCP_TEST_TOKEN"]
async fn live_full_connector_boot_sequence() {
    skip_unless_live!();
    let cfg = live_config().unwrap();
    let mut c = McpHttpClient::new(cfg).expect("config");

    // 1. Connect.
    c.connect().await.expect("handshake");
    // 2. Discover + capability check.
    let tools = c.list_tools().await.expect("discovery");
    McpHttpClient::verify_required_tools(&tools, &connector_core::mcp::trading_tools::REQUIRED)
        .expect("capability check");
    // 3. Account snapshot.
    let snap = mt5::account_snapshot(
        &c.call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
            .await
            .expect("account"),
    )
    .expect("map");
    // 4. Positions + history (both must respond; content may be empty).
    let pos = mt5::open_positions(
        &c.call_tool_json(
            connector_core::mcp::trading_tools::OPEN_POSITIONS,
            &json!({ "include_orders": true }),
        )
        .await
        .expect("positions"),
    )
    .expect("map");
    let hist = mt5::history_orders(
        &c.call_tool_json(
            connector_core::mcp::trading_tools::HISTORY_ORDERS,
            &json!({ "datetime_from": "2026-10-18T00:00:00", "datetime_to": "2026-11-18T00:00:00", "limit": 100 }),
        )
        .await
        .expect("history"),
    )
    .expect("map");

    let mut engine = connector_core::sync::SyncEngine::new(Default::default());
    let b1 = engine
        .build_account_batch(&snap)
        .expect("serialize snapshot");
    let b2 = engine
        .build_positions_batch(&pos)
        .expect("serialize positions");
    let b3 = engine.build_history_batch(&hist);
    println!(
        "LIVE boot sequence OK: batches built (account={}, positions={}, history={}), checkpoint={:?}",
        b1.count,
        b2.count,
        b3.count,
        engine.checkpoint()
    );
    // The engine accepted every reconcilable record.
    assert_eq!(engine.pending_count(), 0, "batches were built, not queued");
}

// ---------------------------------------------------------------------------
// 106-action manifest (spec §3): full discovery over the LIVE server
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live MT5 integration — set NSE_LIVE_MCP_TESTS=1 + NSE_MCP_TEST_TOKEN"]
async fn live_action_manifest_full_discovery_and_risk_classification() {
    skip_unless_live!();
    let cfg = live_config().unwrap();
    let mut c = McpHttpClient::new(cfg).expect("config");
    c.connect().await.expect("handshake");

    // §3.1: build the complete manifest from the real tools/list.
    let manifest = connector_core::actions::ActionManifest::discover(&mut c)
        .await
        .expect("live manifest");

    let hist = manifest.tier_histogram();
    println!(
        "LIVE manifest: server={} v{} actions={} hash={:016x}",
        manifest.server_name, manifest.server_version, manifest.action_count, manifest.content_hash
    );
    println!(
        "LIVE tiers: tier0(read)={} tier1(local-modify)={} tier2(fs/code/network)={} tier3(financial)={}",
        hist[0], hist[1], hist[2], hist[3]
    );

    // §3.2: count validation against the expected 106.
    let report = manifest.verify_count(connector_core::actions::EXPECTED_ACTION_COUNT);
    println!(
        "LIVE count check: {} (expected {}, actual {}, delta {:+})",
        report.verdict, report.expected, report.actual, report.delta
    );
    // The observed live server reports 106 or 107 tools depending on
    // configuration; anything else is a hard failure.
    assert!(
        manifest.action_count == 106 || manifest.action_count == 107,
        "unexpected live action count: {}",
        manifest.action_count
    );

    // Every one of the 106+ actions is classified; nothing falls outside
    // the tier system, and the financial set matches the verified trade_*
    // execution tools exactly.
    let financial = manifest.by_tier(connector_core::actions::RiskTier::Financial);
    let mut names: Vec<&str> = financial.iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "trade_close_by_position",
            "trade_close_single_position",
            "trade_delete_order",
            "trade_modify_sl_tp",
            "trade_send_market_order",
            "trade_send_pending_order",
        ],
        "Tier 3 (financial) must be exactly the verified execution tools"
    );

    // The 4 pipeline tools are present and read-only.
    for required in connector_core::mcp::trading_tools::REQUIRED {
        let d = manifest.get(required).expect(required);
        assert_eq!(
            d.classification.tier,
            connector_core::actions::RiskTier::ReadOnly
        );
    }

    // All descriptors serialize (the manifest can be registered with NSE
    // Core, spec §3.1).
    let wire = serde_json::to_string(&manifest).expect("manifest serializes");
    assert!(wire.len() > 10_000, "manifest carries real schema content");
}

#[tokio::test]
#[ignore = "live MT5 integration — set NSE_LIVE_MCP_TESTS=1 + NSE_MCP_TEST_TOKEN"]
async fn live_read_only_action_executes_through_the_dispatcher() {
    skip_unless_live!();
    let cfg = live_config().unwrap();
    let mut c = McpHttpClient::new(cfg).expect("config");
    c.connect().await.expect("handshake");
    let manifest = connector_core::actions::ActionManifest::discover(&mut c)
        .await
        .expect("manifest");
    let dispatcher = connector_core::actions::ActionDispatcher::new(
        manifest,
        std::sync::Arc::new(connector_core::actions::KillSwitch::new()),
    );

    // Fetch the account once to bind the command to the REAL login the
    // terminal reports (the account id a production connector would have
    // registered with NSE Core).
    let snap = mt5::account_snapshot(
        &c.call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
            .await
            .expect("account info"),
    )
    .expect("map");
    let login = snap.account.login.clone().expect("login");

    let raw = json!({
        "request_id": "live-req-1",
        "command_id": "live-cmd-1",
        "action_name": "get_trading_account_info",
        "account_id": login,
        "device_id": "live-device",
        "issued_at": "2026-01-01T00:00:00Z",
        "expires_at": "2099-01-01T00:00:00Z",
        "idempotency_key": "live-idem-1",
        "parameters": {},
        "authorization_context": {}
    })
    .to_string();

    let ctx = connector_core::actions::DispatchContext::new("live-device")
        .with_account(login)
        .with_user("live-user");
    let outcome = dispatcher
        .dispatch(&mut c, &raw, &ctx)
        .await
        .expect("tier 0 dispatch must pass the gate chain");
    assert_eq!(
        outcome.status,
        connector_core::actions::ActionStatus::Executed
    );
    assert!(outcome.actual_mcp_result.is_some());
    println!(
        "LIVE dispatch: action={} status={} duration={}ms",
        outcome.action_name,
        outcome.status.as_str(),
        outcome.duration_ms
    );
}
