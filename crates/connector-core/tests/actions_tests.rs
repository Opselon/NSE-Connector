//! 106-action architecture integration tests (spec §3/§6/§7): manifest
//! discovery over the wire, count validation, universal dispatcher gate
//! chain, contract envelopes, kill switch, symbol registry.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use common::account_payload;
use common::mock_mcp::{MockMcp, ToolBehavior};
use connector_core::actions::dispatcher::{
    ActionDispatcher, ActionRequest, ActionStatus, DispatchContext, DispatchRejection,
};
use connector_core::actions::kill::{KillScope, KillSwitch};
use connector_core::actions::manifest::{ActionManifest, CountReport, EXPECTED_ACTION_COUNT};
use connector_core::actions::risk::RiskTier;
use connector_core::actions::{classify, validate_against_schema};
use connector_core::mcp::{McpClientConfig, McpHttpClient};
use connector_core::security::secrets::Secret;
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

async fn connected_client(mock: &MockMcp) -> McpHttpClient {
    let mut c = mcp_client(&mock.endpoint());
    c.connect().await.expect("handshake");
    c
}

/// Builds a manifest from the mock's actual tools/list — the same path the
/// connector uses in production.
async fn discover(mock: &MockMcp, client: &mut McpHttpClient) -> ActionManifest {
    let _ = mock;
    ActionManifest::discover(client)
        .await
        .expect("manifest discovery")
}

fn request(action: &str, params: serde_json::Value, id: &str) -> String {
    json!({
        "request_id": format!("req-{id}"),
        "command_id": format!("cmd-{id}"),
        "action_name": action,
        "account_id": "12345678901",
        "device_id": "device-test-1",
        "issued_at": "2026-11-17T00:00:00Z",
        "expires_at": "2099-01-01T00:00:00Z",
        "idempotency_key": id,
        "parameters": params,
        "authorization_context": { "user_id": "user-123" }
    })
    .to_string()
}

/// Tool set that additionally registers a Tier 1, a Tier 2, and a Tier 3
/// action (as real MT5 MCP servers do) with strict schemas.
fn extended_tools() -> serde_json::Value {
    let strict = |name: &str| {
        json!({
            "name": name,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string" },
                    "path": { "type": "string" },
                    "ticket": { "type": "string" }
                },
                "required": [],
                "additionalProperties": false
            }
        })
    };
    json!([
        strict(connector_core::mcp::trading_tools::ACCOUNT_INFO),
        strict(connector_core::mcp::trading_tools::OPEN_POSITIONS),
        strict(connector_core::mcp::trading_tools::HISTORY_ORDERS),
        strict(connector_core::mcp::trading_tools::HISTORY_POSITIONS),
        strict("chart_open"),
        strict("write_file"),
        strict("trade_close_single_position")
    ])
}

fn full_ctx() -> DispatchContext {
    DispatchContext::new("device-test-1")
        .with_account("12345678901")
        .with_user("user-123")
        .with_grant("local.modify")
        .with_grant("advanced.execute")
        .with_grant("trading.execute")
        .with_financial_consent(true)
}

// ---------------------------------------------------------------------------
// Manifest discovery + count validation (spec §3.1/§3.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manifest_is_built_dynamically_from_the_real_tools_list() {
    let mock = MockMcp::spawn().await;
    mock.set_tools(extended_tools()).await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    assert_eq!(manifest.server_name, "MockMT5-MCP");
    assert_eq!(manifest.action_count, 7);
    // Every descriptor carries its schema and permission requirement.
    let acc = manifest
        .get(connector_core::mcp::trading_tools::ACCOUNT_INFO)
        .expect("account info registered");
    assert_eq!(acc.classification.tier, RiskTier::ReadOnly);
    assert_eq!(acc.required_permission, "");
    let trade = manifest
        .get("trade_close_single_position")
        .expect("registered");
    assert_eq!(trade.classification.tier, RiskTier::Financial);
    assert_eq!(trade.required_permission, "trading.execute");
    mock.stop().await;
}

#[tokio::test]
async fn count_validation_reports_the_three_cases() {
    let mock = MockMcp::spawn().await;
    mock.set_tools(extended_tools()).await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;

    // Fewer than expected (7 tools on the mock vs the expected set).
    let report = manifest.verify_count(EXPECTED_ACTION_COUNT);
    assert_eq!(
        report,
        CountReport {
            expected: EXPECTED_ACTION_COUNT,
            actual: 7,
            delta: 7i64 - EXPECTED_ACTION_COUNT as i64,
            verdict: "fewer_than_expected",
        }
    );

    // As expected.
    assert_eq!(manifest.verify_count(7).verdict, "as_expected");

    // More than expected (spec: register the extra actions without breaking
    // compatibility).
    assert_eq!(manifest.verify_count(2).verdict, "more_than_expected");
    mock.stop().await;
}

#[tokio::test]
async fn server_without_required_tools_reports_exact_missing_set() {
    // Restricted installation: only two trading tools available.
    let mock = MockMcp::spawn().await;
    mock.set_tools(json!([
        { "name": connector_core::mcp::trading_tools::ACCOUNT_INFO },
        { "name": connector_core::mcp::trading_tools::OPEN_POSITIONS }
    ]))
    .await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let full = full_baseline();
    let missing = manifest.missing_since(&full);
    assert!(missing.contains(&connector_core::mcp::trading_tools::HISTORY_ORDERS.to_string()));
    assert!(missing.contains(&connector_core::mcp::trading_tools::HISTORY_POSITIONS.to_string()));
    assert_eq!(missing.len(), 2, "exact difference reported");
    mock.stop().await;
}

fn full_baseline() -> ActionManifest {
    let mut actions = std::collections::BTreeMap::new();
    for name in connector_core::mcp::trading_tools::REQUIRED {
        let classification = classify(name, "");
        actions.insert(
            name.to_string(),
            connector_core::actions::ActionDescriptor {
                name: name.to_string(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
                output_schema: None,
                required_permission: classification.tier.required_permission().to_string(),
                classification,
            },
        );
    }
    ActionManifest {
        server_name: "baseline".into(),
        server_version: "1.0".into(),
        discovered_at: String::new(),
        action_count: actions.len(),
        content_hash: 0,
        actions,
    }
}

// ---------------------------------------------------------------------------
// Universal dispatcher — gate chain (spec §3.3/§7)
// ---------------------------------------------------------------------------

fn dispatcher_with(manifest: ActionManifest) -> ActionDispatcher {
    ActionDispatcher::new(manifest, Arc::new(KillSwitch::new()))
}

#[tokio::test]
async fn tier0_action_executes_and_returns_the_real_mcp_payload() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::TextJson(account_payload()),
    )
    .await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let dispatcher = dispatcher_with(manifest);

    let outcome = dispatcher
        .dispatch(
            &mut client,
            &request("get_trading_account_info", json!({}), "t0-1"),
            &full_ctx(),
        )
        .await
        .expect("tier 0 needs no grants");
    assert_eq!(outcome.status, ActionStatus::Executed);
    assert_eq!(
        outcome
            .actual_mcp_result
            .expect("payload")
            .pointer("/account/balance"),
        Some(&json!(26237.73))
    );
    assert_eq!(outcome.correlation_id, "req-t0-1");
    assert!(outcome.duration_ms < 5_000);
    mock.stop().await;
}

#[tokio::test]
async fn unknown_action_is_never_executed() {
    let mock = MockMcp::spawn().await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let dispatcher = dispatcher_with(manifest);

    let err = dispatcher
        .dispatch(
            &mut client,
            &request("brand_new_action", json!({}), "u1"),
            &full_ctx(),
        )
        .await
        .expect_err("unregistered action must be rejected");
    assert!(matches!(err, DispatchRejection::UnknownAction(n) if n == "brand_new_action"));
    mock.stop().await;
}

#[tokio::test]
async fn tier_gates_require_their_specific_grant() {
    let mock = MockMcp::spawn().await;
    mock.set_tools(extended_tools()).await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let dispatcher = dispatcher_with(manifest);

    // Tier 1 without "local.modify" -> PermissionDenied.
    let ctx_no_t1 = DispatchContext::new("device-test-1")
        .with_account("12345678901")
        .with_user("user-123");
    let err = dispatcher
        .dispatch(
            &mut client,
            &request("chart_open", json!({}), "g1"),
            &ctx_no_t1,
        )
        .await
        .expect_err("tier 1 needs local.modify");
    assert!(matches!(
        err,
        DispatchRejection::PermissionDenied {
            tier: RiskTier::LocalModification,
            ..
        }
    ));

    // Tier 2 without "advanced.execute" -> PermissionDenied.
    let err = dispatcher
        .dispatch(
            &mut client,
            &request("write_file", json!({ "path": "x.txt" }), "g2"),
            &ctx_no_t1,
        )
        .await
        .expect_err("tier 2 needs advanced.execute");
    assert!(matches!(
        err,
        DispatchRejection::PermissionDenied {
            tier: RiskTier::FilesystemOrCode,
            ..
        }
    ));

    // Tier 3 needs "trading.execute" even with everything else granted.
    let ctx_t1_t2 = DispatchContext::new("device-test-1")
        .with_account("12345678901")
        .with_user("user-123")
        .with_grant("local.modify")
        .with_grant("advanced.execute")
        .with_financial_consent(true);
    let err = dispatcher
        .dispatch(
            &mut client,
            &request(
                "trade_close_single_position",
                json!({ "ticket": "1" }),
                "g3",
            ),
            &ctx_t1_t2,
        )
        .await
        .expect_err("tier 3 needs trading.execute");
    assert!(matches!(
        err,
        DispatchRejection::PermissionDenied {
            tier: RiskTier::Financial,
            ..
        }
    ));
    mock.stop().await;
}

#[tokio::test]
async fn tier3_without_explicit_customer_consent_never_executes() {
    let mock = MockMcp::spawn().await;
    mock.set_tools(extended_tools()).await;
    mock.set_tool_behavior(
        "trade_close_single_position",
        ToolBehavior::TextJson(json!({ "retcode": "done" })),
    )
    .await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let dispatcher = dispatcher_with(manifest);

    // Every grant present, but the customer consent flag is false.
    let ctx_no_consent = DispatchContext::new("device-test-1")
        .with_account("12345678901")
        .with_user("user-123")
        .with_grant("trading.execute")
        .with_financial_consent(false);
    let err = dispatcher
        .dispatch(
            &mut client,
            &request(
                "trade_close_single_position",
                json!({ "ticket": "1" }),
                "c1",
            ),
            &ctx_no_consent,
        )
        .await
        .expect_err("consent gate");
    assert_eq!(err, DispatchRejection::FinancialConsentMissing);

    // With consent: executes and reports the real MCP result.
    let outcome = dispatcher
        .dispatch(
            &mut client,
            &request(
                "trade_close_single_position",
                json!({ "ticket": "1" }),
                "c2",
            ),
            &full_ctx(),
        )
        .await
        .expect("executes with consent");
    assert_eq!(outcome.status, ActionStatus::Executed);
    mock.stop().await;
}

#[tokio::test]
async fn expired_malformed_and_misbound_commands_are_rejected() {
    let mock = MockMcp::spawn().await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let dispatcher = dispatcher_with(manifest);
    let ctx = full_ctx();

    // Expired (a date long in the past — the machine clock must never
    // resurrect an expired command).
    let expired = json!({
        "request_id": "r", "command_id": "c", "action_name": "get_trading_account_info",
        "account_id": "12345678901", "device_id": "device-test-1",
        "issued_at": "2020-01-01T00:00:00Z", "expires_at": "2020-01-02T00:00:00Z",
        "idempotency_key": "e1", "parameters": {}, "authorization_context": {}
    })
    .to_string();
    assert_eq!(
        dispatcher
            .dispatch(&mut client, &expired, &ctx)
            .await
            .unwrap_err(),
        DispatchRejection::Expired
    );

    // Malformed.
    assert_eq!(
        dispatcher
            .dispatch(&mut client, "not json", &ctx)
            .await
            .unwrap_err(),
        DispatchRejection::Malformed
    );

    // Wrong device.
    let other_device = request("get_trading_account_info", json!({}), "d1")
        .replace("\"device-test-1\"", "\"device-999\"");
    assert_eq!(
        dispatcher
            .dispatch(&mut client, &other_device, &ctx)
            .await
            .unwrap_err(),
        DispatchRejection::WrongDevice
    );

    // Wrong account (cross-account isolation).
    let other_account =
        request("get_trading_account_info", json!({}), "d2").replace("\"12345678901\"", "\"999\"");
    assert_eq!(
        dispatcher
            .dispatch(&mut client, &other_account, &ctx)
            .await
            .unwrap_err(),
        DispatchRejection::WrongAccount
    );
    mock.stop().await;
}

#[tokio::test]
async fn schema_violations_reject_before_execution() {
    let mock = MockMcp::spawn().await;
    mock.set_tools(extended_tools()).await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::TextJson(json!({ "never": "reached" })),
    )
    .await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let dispatcher = dispatcher_with(manifest);

    // The mock's account-info schema allows only an empty object; injecting
    // a parameter must fail validation (additionalProperties: false).
    let err = dispatcher
        .dispatch(
            &mut client,
            &request("get_trading_account_info", json!({ "evil": true }), "s1"),
            &full_ctx(),
        )
        .await
        .expect_err("unknown parameter must be rejected");
    let DispatchRejection::SchemaValidation(errors) = err else {
        panic!("expected schema rejection");
    };
    assert!(errors.iter().any(|e| e.contains("evil")), "{errors:?}");
    mock.stop().await;
}

#[tokio::test]
async fn replayed_commands_are_rejected_after_success() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::TextJson(account_payload()),
    )
    .await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let dispatcher = dispatcher_with(manifest);
    let ctx = full_ctx();

    let raw = request("get_trading_account_info", json!({}), "same-id");
    let first = dispatcher
        .dispatch(&mut client, &raw, &ctx)
        .await
        .expect("first");
    assert_eq!(first.status, ActionStatus::Executed);
    let err = dispatcher
        .dispatch(&mut client, &raw, &ctx)
        .await
        .expect_err("replay");
    assert_eq!(err, DispatchRejection::Replayed);

    // A rejected command does NOT burn its idempotency id: after an
    // unknown-action rejection, a valid command with the same id still runs.
    let _ = dispatcher
        .dispatch(&mut client, &request("nope", json!({}), "reuse"), &ctx)
        .await
        .unwrap_err();
    let ok = dispatcher
        .dispatch(
            &mut client,
            &request("get_trading_open_positions", json!({}), "reuse"),
            &ctx,
        )
        .await
        .expect("id reusable after rejection");
    assert_eq!(ok.status, ActionStatus::Executed);
    mock.stop().await;
}

#[tokio::test]
async fn kill_switch_blocks_new_commands_in_scope() {
    let mock = MockMcp::spawn().await;
    mock.set_tools(extended_tools()).await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::TextJson(json!({ "account": {} })),
    )
    .await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let kill = Arc::new(KillSwitch::new());
    let dispatcher = ActionDispatcher::new(manifest, Arc::clone(&kill));
    let ctx = full_ctx();

    // Global kill.
    kill.engage_all();
    assert!(matches!(
        dispatcher
            .dispatch(
                &mut client,
                &request("get_trading_account_info", json!({}), "k1"),
                &ctx
            )
            .await
            .unwrap_err(),
        DispatchRejection::KillSwitchActive(KillScope::Global)
    ));
    kill.lift(&KillScope::Global);

    // Account-scoped kill.
    kill.engage(&KillScope::Account("12345678901".into()));
    assert!(matches!(
        dispatcher
            .dispatch(
                &mut client,
                &request("get_trading_account_info", json!({}), "k2"),
                &ctx
            )
            .await
            .unwrap_err(),
        DispatchRejection::KillSwitchActive(KillScope::Account(_))
    ));
    kill.lift(&KillScope::Account("12345678901".into()));

    // Auto-trade kill only gates Tier 3.
    kill.engage(&KillScope::AutoTrade);
    let t0 = dispatcher
        .dispatch(
            &mut client,
            &request("get_trading_account_info", json!({}), "k3"),
            &ctx,
        )
        .await;
    assert!(t0.is_ok(), "tier 0 unaffected by auto-trade switch");
    let t3 = dispatcher
        .dispatch(
            &mut client,
            &request(
                "trade_close_single_position",
                json!({ "ticket": "1" }),
                "k4",
            ),
            &ctx,
        )
        .await;
    assert!(matches!(
        t3.unwrap_err(),
        DispatchRejection::KillSwitchActive(KillScope::AutoTrade)
    ));
    mock.stop().await;
}

#[tokio::test]
async fn manifest_refresh_changes_dispatch_behavior() {
    let mock = MockMcp::spawn().await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let dispatcher = dispatcher_with(manifest);

    // Server gains a new capability: replace the manifest, action becomes
    // executable; the old manifest would have rejected it.
    mock.set_tools(json!([
        { "name": "brand_new_action", "inputSchema": { "type": "object", "properties": {}, "required": [] } }
    ]))
    .await;
    let refreshed = discover(&mock, &mut client).await;
    dispatcher.replace_manifest(refreshed);

    let outcome = dispatcher
        .dispatch(
            &mut client,
            &request("brand_new_action", json!({}), "r1"),
            &full_ctx(),
        )
        .await
        .expect("registered after refresh");
    assert_eq!(outcome.status, ActionStatus::Executed);
    // Fail-closed default tier for an unclassified action: required grant.
    mock.stop().await;
}

#[tokio::test]
async fn mcp_failure_is_reported_as_mcp_failed_not_executed() {
    let mock = MockMcp::spawn().await;
    mock.set_tool_behavior(
        connector_core::mcp::trading_tools::ACCOUNT_INFO,
        ToolBehavior::IsError("terminal not running".into()),
    )
    .await;
    let mut client = connected_client(&mock).await;
    let manifest = discover(&mock, &mut client).await;
    let dispatcher = dispatcher_with(manifest);

    let outcome = dispatcher
        .dispatch(
            &mut client,
            &request("get_trading_account_info", json!({}), "f1"),
            &full_ctx(),
        )
        .await
        .expect("authorization passed");
    assert_eq!(outcome.status, ActionStatus::McpFailed);
    assert!(outcome.execution_error.is_some());
    assert!(outcome.actual_mcp_result.is_none());
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Contract envelopes (spec §6)
// ---------------------------------------------------------------------------

#[test]
fn action_result_envelope_carries_the_contract_fields() {
    use connector_core::actions::{DataQuality, MessageContext, MessageEnvelope, SyncMetadata};
    let e = MessageEnvelope::new(
        MessageContext {
            event_type: "sync.batch",
            device_id: "device-test-1",
            user_id: Some("user-123"),
            account_id: "12345678901",
            broker_id: Some("MetaQuotes Ltd."),
            terminal_id: None,
            source_tool: Some("get_trading_account_info"),
            correlation_id: Some("corr-42"),
        },
        json!({ "account": { "balance": 26237.73 } }),
        DataQuality {
            validation_warnings: vec![],
            raw_available: true,
        },
        SyncMetadata {
            checkpoint: Some("2026-11-16T22:33:39".into()),
            batch_kind: Some("account".into()),
        },
    );
    let wire = serde_json::to_value(&e).expect("serialize");
    // §6 mandatory fields.
    for field in [
        "protocol_version",
        "schema_version",
        "message_id",
        "event_type",
        "event_timestamp",
        "device_id",
        "account_id",
        "broker_id",
        "source_tool",
        "payload",
        "data_quality",
        "sync_metadata",
    ] {
        assert!(wire.get(field).is_some(), "missing {field}");
    }
    assert_eq!(
        wire["sync_metadata"]["checkpoint"],
        json!("2026-11-16T22:33:39")
    );
}

// ---------------------------------------------------------------------------
// Symbol registry (spec §5)
// ---------------------------------------------------------------------------

#[test]
fn symbol_registry_preserves_broker_names_for_calls() {
    use connector_core::actions::SymbolRegistry;
    let mut reg = SymbolRegistry::new();
    // A broker whose gold symbol is XAUUSDm and NAS100 is called USTEC.
    reg.register("XAUUSDm", json!({ "digits": 2 }), "2026-11-17T00:00:00Z");
    reg.register("USTEC", json!({}), "2026-11-17T00:00:00Z");
    let known: Vec<String> = vec!["XAUUSD".into(), "EURUSD".into(), "NAS100".into()];
    reg.resolve_canonicals(&known);

    // XAUUSDm resolves confidently; USTEC does NOT map to NAS100 (different
    // name, not a decoration) — never guessed.
    assert_eq!(
        reg.get("XAUUSDm").unwrap().canonical.as_deref(),
        Some("XAUUSD")
    );
    assert_eq!(reg.get("USTEC").unwrap().canonical, None);
    // Calls always use the ORIGINAL broker symbol.
    assert_eq!(reg.call_symbol("XAUUSDm"), "XAUUSDm");
}

// ---------------------------------------------------------------------------
// ActionRequest deserialization (spec §6.2)
// ---------------------------------------------------------------------------

#[test]
fn action_request_round_trips_the_section_62_fields() {
    let raw = request(
        "get_trading_open_positions",
        json!({ "include_orders": true }),
        "ar1",
    );
    let req: ActionRequest = serde_json::from_str(&raw).expect("parse");
    assert_eq!(req.action_name, "get_trading_open_positions");
    assert_eq!(req.idempotency_key, "ar1");
    assert_eq!(req.parameters["include_orders"], json!(true));
}

#[test]
fn schema_validator_rejects_wrong_required_types() {
    let schema = json!({
        "type": "object",
        "properties": { "symbol": { "type": "string" }, "limit": { "type": "integer" } },
        "required": ["symbol"],
        "additionalProperties": false
    });
    let grants: HashSet<String> = HashSet::new();
    let _ = grants; // silence unused in some cfgs
    assert!(
        validate_against_schema(&json!({ "symbol": "EURUSD", "limit": 5 }), &schema).is_empty()
    );
    let errs = validate_against_schema(&json!({ "limit": 5 }), &schema);
    assert!(errs.iter().any(|e| e.contains("symbol: required")));
    let errs = validate_against_schema(&json!({ "symbol": 5 }), &schema);
    assert!(errs.iter().any(|e| e.contains("symbol: expected string")));
}
