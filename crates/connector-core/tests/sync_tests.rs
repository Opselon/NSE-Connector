//! Synchronization tests (engineering spec §16 "Synchronization Tests") —
//! every bullet mapped to a named test. Uses the mock MCP server for MT5
//! payloads and the mock NSE Core for delivery/acknowledgment.

mod common;

use std::time::Duration;

use common::mock_mcp::{MockMcp, ToolBehavior};
use common::mock_nse::MockNse;
use common::{account_payload, history_payload, positions_payload};
use connector_core::mcp::{McpClientConfig, McpHttpClient};
use connector_core::mt5;
use connector_core::nse::SyncBatchRequest;
use connector_core::nse::{NseGatewayClient, NseGatewayConfig};
use connector_core::security::secrets::Secret;
use connector_core::sync::{BatchKind, SyncConfig, SyncEngine};
use serde_json::json;

fn mcp_client(endpoint: &str) -> McpHttpClient {
    McpHttpClient::new(McpClientConfig {
        endpoint: endpoint.to_string(),
        bearer: Some(Secret::new("test-token")),
        request_timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .expect("client config")
}

fn nse_client(mock: &MockNse) -> NseGatewayClient {
    NseGatewayClient::new(NseGatewayConfig {
        base_url: mock.base_url(),
        api_key: Secret::new("NSE-key-valid"),
        device_id: "device-test-1".to_string(),
        timeout: Duration::from_secs(5),
    })
    .expect("gateway config")
}

/// Full round trip for one tool: fetch via MCP, map, build a batch, deliver,
/// ack. Returns the batch (recorded bodies are read from the mock directly).
async fn fetch_build_deliver(
    mcp: &mut McpHttpClient,
    tool: &str,
    engine: &mut SyncEngine,
    nse: &NseGatewayClient,
) -> connector_core::sync::SyncBatch {
    let payload = mcp
        .call_tool_json(tool, &json!({}))
        .await
        .expect("tool call");
    let batch = match tool {
        connector_core::mcp::trading_tools::ACCOUNT_INFO => {
            let snap = mt5::account_snapshot(&payload).expect("map");
            engine.build_account_batch(&snap)
        }
        connector_core::mcp::trading_tools::OPEN_POSITIONS => {
            let pos = mt5::open_positions(&payload).expect("map");
            engine.build_positions_batch(&pos)
        }
        connector_core::mcp::trading_tools::HISTORY_ORDERS => {
            let hist = mt5::history_orders(&payload).expect("map");
            engine.build_history_batch(&hist)
        }
        _ => unreachable!("test only uses three tools"),
    };
    // Realistic pipeline: build -> queue -> deliver -> ack -> remove.
    engine.queue(batch.clone());
    let req = SyncBatchRequest {
        batch_id: batch.batch_id.clone(),
        kind: batch.kind.as_str().to_string(),
        items: batch.items.clone(),
        count: batch.count as u32,
        produced_at: batch.produced_at.clone(),
    };
    let ack = nse.send_batch(&req).await.expect("delivery");
    assert!(ack.accepted);
    assert!(
        engine.mark_acked(&batch.batch_id),
        "ack must remove the queued batch"
    );
    batch
}

// ---------------------------------------------------------------------------
// Account snapshots / open positions / historical deals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn account_snapshot_round_trips_to_nse() {
    let mock_mcp = MockMcp::spawn().await;
    let mock_nse = MockNse::spawn().await;
    mock_mcp
        .set_tool_behavior(
            connector_core::mcp::trading_tools::ACCOUNT_INFO,
            ToolBehavior::TextJson(account_payload()),
        )
        .await;
    let mut mcp = mcp_client(&mock_mcp.endpoint());
    let nse = nse_client(&mock_nse);
    let mut engine = SyncEngine::new(SyncConfig::default());

    let batch = fetch_build_deliver(
        &mut mcp,
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        &mut engine,
        &nse,
    )
    .await;
    assert_eq!(batch.kind, BatchKind::Account);

    let recorded = mock_nse.recorded().await;
    let batch_req = recorded
        .iter()
        .find(|r| r.path == "/api/v1/connector/sync/batch")
        .expect("batch delivered");
    assert_eq!(batch_req.body["kind"], "account");
    assert_eq!(
        batch_req.body["items"]["account"]["balance"],
        json!(26237.73)
    );
    assert_eq!(batch_req.body["items"]["account"]["currency"], "USD");
    // Broker terminal timestamp preserved verbatim.
    assert_eq!(
        batch_req.body["items"]["terminal"]["timestamp"],
        "2026-11-16T20:31:21Z"
    );

    // The helper already queued, delivered, and acked: pending must be empty.
    assert_eq!(engine.pending_count(), 0);
    mock_mcp.stop().await;
    mock_nse.stop().await;
}

#[tokio::test]
async fn open_positions_round_trip_with_counts() {
    let mock_mcp = MockMcp::spawn().await;
    let mock_nse = MockNse::spawn().await;
    mock_mcp
        .set_tool_behavior(
            connector_core::mcp::trading_tools::OPEN_POSITIONS,
            ToolBehavior::TextJson(positions_payload()),
        )
        .await;
    let mut mcp = mcp_client(&mock_mcp.endpoint());
    let nse = nse_client(&mock_nse);
    let mut engine = SyncEngine::new(SyncConfig::default());

    let batch = fetch_build_deliver(
        &mut mcp,
        connector_core::mcp::trading_tools::OPEN_POSITIONS,
        &mut engine,
        &nse,
    )
    .await;
    assert_eq!(batch.kind, BatchKind::Positions);
    assert_eq!(batch.count, 3, "2 positions + 1 order");
    let recorded = mock_nse.recorded().await;
    let body = &recorded
        .iter()
        .find(|r| r.path == "/api/v1/connector/sync/batch")
        .expect("delivered")
        .body;
    assert_eq!(
        body["items"]["positions"][0]["position_id"],
        json!("152913954001")
    );
    assert_eq!(
        body["items"]["orders"][0]["order_id"],
        json!("152913954010")
    );
    assert_eq!(
        body["items"]["positions"][0]["create_time"],
        "2026-11-16T22:33:39"
    );
    mock_mcp.stop().await;
    mock_nse.stop().await;
}

#[tokio::test]
async fn history_round_trip_dedup_and_checkpoint() {
    let mock_mcp = MockMcp::spawn().await;
    let mock_nse = MockNse::spawn().await;
    mock_mcp
        .set_tool_behavior(
            connector_core::mcp::trading_tools::HISTORY_ORDERS,
            ToolBehavior::TextJson(history_payload()),
        )
        .await;
    let mut mcp = mcp_client(&mock_mcp.endpoint());
    let nse = nse_client(&mock_nse);
    let mut engine = SyncEngine::new(SyncConfig::default());

    let batch = fetch_build_deliver(
        &mut mcp,
        connector_core::mcp::trading_tools::HISTORY_ORDERS,
        &mut engine,
        &nse,
    )
    .await;
    assert_eq!(batch.kind, BatchKind::HistoryOrders);
    assert_eq!(batch.count, 3, "1 order + 2 deals");
    // Checkpoint advances to the newest deal time (broker time, verbatim).
    assert_eq!(engine.checkpoint(), Some("2026-11-16T23:30:00"));
    mock_mcp.stop().await;
    mock_nse.stop().await;
}

// ---------------------------------------------------------------------------
// Duplicate detection / incremental sync / recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_deals_are_not_resynchronized() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    let hist: connector_core::mt5::HistoryOrdersResponse =
        serde_json::from_value(history_payload()).expect("payload");
    let first = engine.build_history_batch(&hist);
    assert_eq!(first.count, 3);
    let second = engine.build_history_batch(&hist);
    assert_eq!(second.count, 0, "same deals must dedup to an empty batch");
}

#[tokio::test]
async fn incremental_sync_queries_from_checkpoint() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    // First sync: full history from the beginning of time.
    let (from, _) = engine.history_query_range("2026-11-17T00:00:00Z");
    assert_eq!(from, "1970-01-01T00:00:00Z");

    let hist: connector_core::mt5::HistoryOrdersResponse =
        serde_json::from_value(history_payload()).expect("payload");
    let _ = engine.build_history_batch(&hist);
    let (from2, to2) = engine.history_query_range("2026-11-17T00:00:00Z");
    assert_eq!(
        from2, "2026-11-16T23:30:00",
        "incremental range starts at checkpoint"
    );
    assert_eq!(to2, "2026-11-17T00:00:00Z");
}

#[tokio::test]
async fn recovery_after_downtime_fills_the_gap() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    // Pre-downtime state: data up to 2026-11-16T23:30:00 (terminal-local,
    // live format without a timezone suffix).
    let hist: connector_core::mt5::HistoryOrdersResponse =
        serde_json::from_value(history_payload()).expect("payload");
    let _ = engine.build_history_batch(&hist);

    // Downtime happens; on recovery the query range must cover the gap
    // (checkpoint -> now), never restart from zero and never skip ahead.
    let (from, to) = engine.history_query_range("2026-11-17T08:00:00Z");
    assert_eq!(from, "2026-11-16T23:30:00");
    assert_eq!(to, "2026-11-17T08:00:00Z");
}

#[tokio::test]
async fn delayed_out_of_order_deals_keep_checkpoint_monotonic() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    // Latest deal first (normal order), then a delayed older deal with a
    // fresh ticket (e.g. recovered after downtime).
    let late_arrival: connector_core::mt5::HistoryOrdersResponse = serde_json::from_value(json!({
        "orders": [],
        "deals": [
            { "ticket": "7000010", "executed_at": "2026-11-16T09:00:00Z", "profit": 1.0 }
        ]
    }))
    .expect("payload");
    let batch = engine.build_history_batch(&late_arrival);
    assert_eq!(
        batch.count, 1,
        "unseen deal is always forwarded, not dropped"
    );
    assert_eq!(engine.checkpoint(), Some("2026-11-16T09:00:00Z"));

    let newer: connector_core::mt5::HistoryOrdersResponse = serde_json::from_value(json!({
        "orders": [],
        "deals": [
            { "ticket": "7000011", "executed_at": "2026-11-16T12:00:00Z", "profit": 2.0 }
        ]
    }))
    .expect("payload");
    let _ = engine.build_history_batch(&newer);
    assert_eq!(engine.checkpoint(), Some("2026-11-16T12:00:00Z"));

    // Now a second delayed deal older than the checkpoint: included, but the
    // checkpoint never moves backwards.
    let older: connector_core::mt5::HistoryOrdersResponse = serde_json::from_value(json!({
        "orders": [],
        "deals": [
            { "ticket": "7000012", "executed_at": "2026-11-16T08:00:00Z", "profit": 3.0 }
        ]
    }))
    .expect("payload");
    let batch = engine.build_history_batch(&older);
    assert_eq!(batch.count, 1);
    assert_eq!(
        engine.checkpoint(),
        Some("2026-11-16T12:00:00Z"),
        "checkpoint stays at max"
    );
}

#[tokio::test]
async fn dealless_records_without_tickets_are_dropped() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    let payload: connector_core::mt5::HistoryOrdersResponse = serde_json::from_value(json!({
        "orders": [ { "symbol": "EURUSD" } ],
        "deals": [ { "symbol": "EURUSD", "profit": 5.0 } ]
    }))
    .expect("payload");
    let batch = engine.build_history_batch(&payload);
    assert_eq!(
        batch.count, 0,
        "unreconcilable records must not be forwarded"
    );
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pagination_uses_configured_page_size_and_ranges() {
    let engine = SyncEngine::new(SyncConfig {
        history_page_limit: 100,
        ..Default::default()
    });
    assert_eq!(engine.history_page_limit(), 100);
    // The page request itself is exercised against the live tool in
    // live_mt5_tests (history range query with limit); here we assert the
    // engine hands out consistent parameters for each page.
    let (from, to) = engine.history_query_range("2026-11-17T00:00:00Z");
    assert!(from < to);
}

#[tokio::test]
async fn paginated_history_pages_accumulate_without_duplicates() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    // Page 1: deals A, B. Page 2 (overlapping window): deals B, C.
    let page1: connector_core::mt5::HistoryOrdersResponse = serde_json::from_value(json!({
        "orders": [],
        "deals": [
            { "ticket": "1", "executed_at": "2026-11-16T10:00:00Z" },
            { "ticket": "2", "executed_at": "2026-11-16T10:05:00Z" }
        ]
    }))
    .expect("payload");
    let page2: connector_core::mt5::HistoryOrdersResponse = serde_json::from_value(json!({
        "orders": [],
        "deals": [
            { "ticket": "2", "executed_at": "2026-11-16T10:05:00Z" },
            { "ticket": "3", "executed_at": "2026-11-16T10:10:00Z" }
        ]
    }))
    .expect("payload");
    let b1 = engine.build_history_batch(&page1);
    assert_eq!(b1.count, 2);
    let b2 = engine.build_history_batch(&page2);
    assert_eq!(
        b2.count, 1,
        "overlapping page must only forward the new deal"
    );
}

// ---------------------------------------------------------------------------
// Acknowledgment handling / bounded pending queue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unacknowledged_batches_are_bounded_with_oldest_eviction() {
    let mut engine = SyncEngine::new(SyncConfig {
        max_pending_batches: 3,
        ..Default::default()
    });
    let ids: Vec<String> = (0..5)
        .map(|i| {
            let batch = connector_core::sync::SyncBatch {
                batch_id: format!("batch-{i}"),
                kind: BatchKind::Positions,
                items: json!({}),
                count: 0,
                produced_at: connector_core::sync::SyncEngine::now_rfc3339(),
            };
            let id = batch.batch_id.clone();
            engine.queue(batch);
            id
        })
        .collect();
    assert_eq!(engine.pending_count(), 3, "queue is bounded");
    assert!(
        engine.pending().any(|b| b.batch_id == "batch-4"),
        "newest kept"
    );
    assert!(
        !engine.pending().any(|b| b.batch_id == ids[0]),
        "oldest evicted with warning"
    );
    assert!(!engine.pending().any(|b| b.batch_id == ids[1]));
}

#[tokio::test]
async fn acknowledgment_removes_exactly_the_acked_batch() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    for i in 0..3 {
        engine.queue(connector_core::sync::SyncBatch {
            batch_id: format!("b{i}"),
            kind: BatchKind::Account,
            items: json!({}),
            count: 0,
            produced_at: String::new(),
        });
    }
    assert!(engine.mark_acked("b1"));
    assert!(!engine.mark_acked("b1"), "double-ack is a no-op");
    assert_eq!(engine.pending_count(), 2);
    assert!(engine.pending().all(|b| b.batch_id != "b1"));
}

#[tokio::test]
async fn end_to_end_ack_via_mock_nse() {
    let mock_mcp = MockMcp::spawn().await;
    let mock_nse = MockNse::spawn().await;
    mock_mcp
        .set_tool_behavior(
            connector_core::mcp::trading_tools::ACCOUNT_INFO,
            ToolBehavior::TextJson(account_payload()),
        )
        .await;
    let mut mcp = mcp_client(&mock_mcp.endpoint());
    let nse = nse_client(&mock_nse);
    let mut engine = SyncEngine::new(SyncConfig::default());

    let snap = mt5::account_snapshot(
        &mcp.call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
            .await
            .expect("account"),
    )
    .expect("map");
    let batch = engine.build_account_batch(&snap);
    engine.queue(batch.clone());
    assert_eq!(engine.pending_count(), 1);

    let req = SyncBatchRequest {
        batch_id: batch.batch_id.clone(),
        kind: batch.kind.as_str().to_string(),
        items: batch.items.clone(),
        count: batch.count as u32,
        produced_at: batch.produced_at.clone(),
    };
    let ack = nse.send_batch(&req).await.expect("send");
    assert_eq!(ack.batch_id.as_deref(), Some(batch.batch_id.as_str()));
    assert!(engine.mark_acked(&batch.batch_id));
    assert_eq!(engine.pending_count(), 0);
    mock_mcp.stop().await;
    mock_nse.stop().await;
}

// ---------------------------------------------------------------------------
// Empty account state (live-verified shapes)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_account_state_produces_empty_batches_without_error() {
    let mock_mcp = MockMcp::spawn().await;
    let mock_nse = MockNse::spawn().await;
    mock_mcp
        .set_tool_behavior(
            connector_core::mcp::trading_tools::OPEN_POSITIONS,
            ToolBehavior::TextJson(json!({ "positions": [], "orders": [] })),
        )
        .await;
    mock_mcp
        .set_tool_behavior(
            connector_core::mcp::trading_tools::HISTORY_ORDERS,
            ToolBehavior::TextJson(json!({ "orders": [], "deals": [] })),
        )
        .await;
    let mut mcp = mcp_client(&mock_mcp.endpoint());
    let nse = nse_client(&mock_nse);
    let mut engine = SyncEngine::new(SyncConfig::default());

    for tool in [
        connector_core::mcp::trading_tools::OPEN_POSITIONS,
        connector_core::mcp::trading_tools::HISTORY_ORDERS,
    ] {
        let batch = fetch_build_deliver(&mut mcp, tool, &mut engine, &nse).await;
        assert_eq!(batch.count, 0);
    }
    assert_eq!(engine.checkpoint(), None, "no deals => no checkpoint");
    mock_mcp.stop().await;
    mock_nse.stop().await;
}

// ---------------------------------------------------------------------------
// Timestamp policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn broker_timestamps_are_preserved_verbatim() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    let payload: connector_core::mt5::HistoryOrdersResponse = serde_json::from_value(json!({
        "orders": [],
        "deals": [
            { "ticket": "1", "executed_at": "2026-11-16T09:14:05Z" }
        ]
    }))
    .expect("payload");
    let batch = engine.build_history_batch(&payload);
    let deal = &batch.items["deals"][0];
    assert_eq!(
        deal["open_time"],
        json!("2026-11-16T09:14:05Z"),
        "no re-localization; verbatim broker time preserved"
    );
    assert_eq!(engine.checkpoint(), Some("2026-11-16T09:14:05Z"));
}

#[tokio::test]
async fn alternative_field_names_map_via_alias_layer() {
    // Broker variation law: other MT5 MCP implementations may name the same
    // concepts differently (ticket/deal_id, time/open_time, price/price_open,
    // direction/action). The tolerant models accept both.
    let payload: connector_core::mt5::HistoryOrdersResponse =
        serde_json::from_value(common::history_payload_alt_names()).expect("alt-name payload");
    let mut engine = SyncEngine::new(SyncConfig::default());
    let batch = engine.build_history_batch(&payload);
    assert_eq!(batch.count, 2, "1 order + 1 deal via aliases");
    let deal = &batch.items["deals"][0];
    // Normalized to the live-verified canonical names on output ("action"
    // is the wire name for the Rust field `deal_action`).
    assert_eq!(deal["deal_id"], json!("162000000002"));
    assert_eq!(deal["open_time"], json!("2026-11-16T10:00:00Z"));
    assert_eq!(deal["price_open"], json!(155.5));
    assert_eq!(deal["action"], json!("sell"));
    assert_eq!(engine.checkpoint(), Some("2026-11-16T10:00:00Z"));
}

#[test]
fn dedup_set_is_bounded() {
    let mut set = connector_core::sync::DedupSet::new(3);
    assert!(set.insert("a"));
    assert!(set.insert("b"));
    assert!(set.insert("c"));
    assert!(!set.insert("a"), "dup");
    assert!(set.insert("d"), "eviction makes room");
    assert!(!set.insert("d"));
    assert_eq!(set.len(), 3);
}
