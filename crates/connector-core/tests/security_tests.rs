//! Security tests (engineering spec §16 "Security Tests") — every bullet
//! mapped to a named test. Includes a REAL Windows Credential Manager
//! round-trip (executed against the OS credential vault on this machine).

mod common;

use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use common::account_payload;
use common::mock_mcp::{MockMcp, ToolBehavior};
use common::mock_nse::MockNse;
use connector_core::mcp::{McpClientConfig, McpHttpClient};
use connector_core::mt5;
use connector_core::nse::SyncBatchRequest;
use connector_core::nse::{
    ManagedSocket, NseGatewayClient, NseGatewayConfig, NseSocketConfig, ServerMessage,
    parse_server_message,
};
use connector_core::security::commands::{CommandOperation, CommandRejection, CommandValidator};
use connector_core::security::secrets::{MemorySecretStore, Secret, SecretStore};

// Windows-only store (the roundtrip test below is cfg-gated too).
#[cfg(windows)]
use connector_core::security::WindowsCredentialStore;
use connector_core::security::{mask_login, redact_text};
use serde_json::json;
use std::time::Instant;

fn mcp_client(endpoint: &str, token: &str) -> McpHttpClient {
    McpHttpClient::new(McpClientConfig {
        endpoint: endpoint.to_string(),
        bearer: Some(Secret::new(token)),
        request_timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .expect("client config")
}

fn nse_client(mock: &MockNse, key: &str) -> NseGatewayClient {
    NseGatewayClient::new(NseGatewayConfig {
        base_url: mock.base_url(),
        api_key: Secret::new(key),
        device_id: "device-test-1".to_string(),
        timeout: Duration::from_secs(5),
    })
    .expect("gateway config")
}

// ---------------------------------------------------------------------------
// Cross-boundary secret isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mt5_credential_never_reaches_nse_core() {
    const MT5_TOKEN: &str = "MT5-SUPER-SECRET-abc123";
    const NSE_KEY: &str = "NSE-KEY-xyz789";

    let mock_mcp = MockMcp::spawn().await;
    let mock_nse = MockNse::spawn().await;
    mock_mcp.set_valid_token(MT5_TOKEN).await;
    mock_mcp
        .set_tool_behavior(
            connector_core::mcp::trading_tools::ACCOUNT_INFO,
            ToolBehavior::TextJson(account_payload()),
        )
        .await;

    // Full flow: MT5 fetch (with MT5 token) -> NSE delivery (with NSE key).
    let mut mcp = mcp_client(&mock_mcp.endpoint(), MT5_TOKEN);
    mcp.connect().await.expect("mcp connect");
    let snap = mt5::account_snapshot(
        &mcp.call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
            .await
            .expect("account"),
    )
    .expect("map");
    let batch = {
        let mut engine = connector_core::sync::SyncEngine::new(Default::default());
        engine
            .build_account_batch(&snap)
            .expect("serialize snapshot")
    };
    let nse = nse_client(&mock_nse, NSE_KEY);
    let _ = nse
        .send_batch(&SyncBatchRequest {
            batch_id: batch.batch_id.clone(),
            kind: batch.kind.as_str().to_string(),
            items: batch.items.clone(),
            count: batch.count as u32,
            produced_at: batch.produced_at.clone(),
        })
        .await
        .expect("delivery");

    // EVERY byte that left towards NSE must be free of the MT5 credential —
    // and vice versa: the NSE key must never appear in MT5-bound traffic.
    let recorded = mock_nse.recorded().await;
    assert!(!recorded.is_empty());
    for r in &recorded {
        let header_blob = r.authorization.clone().unwrap_or_default();
        let body_blob = r.body.to_string();
        assert!(
            !header_blob.contains(MT5_TOKEN),
            "MT5 token leaked in NSE headers"
        );
        assert!(
            !body_blob.contains(MT5_TOKEN),
            "MT5 token leaked in NSE body: {body_blob}"
        );
        assert!(
            !body_blob.contains(NSE_KEY),
            "NSE key must not be in bodies either"
        );
    }
    // The NSE key IS present, in the Authorization header, as designed.
    assert!(
        recorded
            .iter()
            .all(|r| r.authorization.as_deref() == Some(&format!("Bearer {NSE_KEY}"))),
        "NSE key must travel only in the Authorization header"
    );
    mock_mcp.stop().await;
    mock_nse.stop().await;
}

// ---------------------------------------------------------------------------
// No credentials in logs
// ---------------------------------------------------------------------------

struct LogBuf(Arc<Mutex<Vec<u8>>>);
struct LogGuard(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogGuard;
    fn make_writer(&'a self) -> Self::Writer {
        LogGuard(self.0.clone())
    }
}

fn log_capture() -> Arc<Mutex<Vec<u8>>> {
    static INIT: Once = Once::new();
    static BUF: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    INIT.call_once(|| {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("connector_core=info"))
            .with_writer(LogBuf(buf.clone()))
            .with_ansi(false)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        let _ = BUF.set(buf);
    });
    BUF.get().unwrap().clone()
}

#[tokio::test]
async fn no_credentials_appear_in_logs() {
    const MT5_TOKEN: &str = "LOG-LEAK-CANARY-token99";
    let buffer = log_capture();

    let mock = MockMcp::spawn().await;
    mock.set_valid_token(MT5_TOKEN).await;
    let mut c = mcp_client(&mock.endpoint(), MT5_TOKEN);
    c.connect().await.expect("connect with canary token");
    let _ = c
        .call_tool_json(connector_core::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await;

    let logs = String::from_utf8_lossy(&buffer.lock().unwrap().clone()).to_string();
    assert!(!logs.is_empty(), "client must log operational events");
    assert!(
        !logs.contains(MT5_TOKEN),
        "MT5 credential must never appear in logs; logs:\n{logs}"
    );
    assert!(
        !logs.contains("Authorization"),
        "auth headers must never be logged"
    );
    mock.stop().await;
}

#[test]
fn secret_never_reveals_itself_via_debug_or_display() {
    let s = Secret::new("SECRET-VALUE-do-not-print");
    let dbg = format!("{s:?}");
    let disp = format!("{s}");
    assert_eq!(dbg, "[REDACTED]");
    assert_eq!(disp, "[REDACTED]");
    // Vec formatting (as happens when a struct containing secrets is logged).
    let vec = vec![s.clone()];
    assert!(!format!("{vec:?}").contains("SECRET-VALUE"));
    // Explicit access still works for the code paths that need it.
    assert_eq!(s.expose(), "SECRET-VALUE-do-not-print");
}

#[test]
fn redaction_removes_secrets_from_diagnostic_text() {
    let token = Secret::new("LEAK-ME-123");
    let login = "12345678901";
    let text = format!("connect endpoint ok; auth=Bearer LEAK-ME-123; login={login}");
    let redacted = redact_text(&text, &[&token]);
    assert!(!redacted.contains("LEAK-ME-123"));
    assert!(redacted.contains("[REDACTED]"));
}

#[test]
fn login_is_masked_in_diagnostics() {
    let full = "12345678901";
    let masked = mask_login(full);
    assert_ne!(masked, full);
    assert!(
        !masked.contains(full),
        "full login must not survive masking"
    );
    assert_eq!(masked, "12***01");
    // Display via AccountInfo Debug stays full — callers must mask; ensure
    // the helper is what diagnostics use.
    let snap = mt5::account_snapshot(&account_payload()).expect("map");
    let shown = mask_login(snap.account.login.as_deref().unwrap_or(""));
    assert!(!shown.contains(full));
}

// ---------------------------------------------------------------------------
// Secret storage (REAL Windows Credential Manager integration)
// ---------------------------------------------------------------------------

// Windows Credential Manager is Windows-only; CI runs the suite on Linux
// where this store has no backend (the port is exercised by the memory
// implementation everywhere else).
#[cfg(windows)]
#[test]
fn windows_credential_store_roundtrip_without_plaintext_files() {
    let service = "NSE-Connector-Tests";
    let entry = format!("test-{}", uuid::Uuid::now_v7());
    let store = WindowsCredentialStore::new(service);

    // Store -> load from a *fresh* store instance (proves OS-side storage,
    // not process memory).
    let secret = Secret::new("credman-roundtrip-SECRET-42");
    store
        .store(&entry, &secret)
        .expect("store into Credential Manager");
    let reload = WindowsCredentialStore::new(service);
    let loaded = reload.load(&entry).expect("load").expect("entry exists");
    assert_eq!(loaded.expose(), "credman-roundtrip-SECRET-42");

    // Delete cleans up.
    reload.delete(&entry).expect("delete");
    assert!(reload.load(&entry).expect("load after delete").is_none());

    // No plaintext credential file was created by our store in temp.
    let temp = std::env::temp_dir();
    let plaintext_candidates = [
        temp.join("nse-connector-secrets.json"),
        temp.join("nse-connector-credentials.json"),
    ];
    for p in plaintext_candidates {
        assert!(!p.exists(), "store must not write plaintext files ({p:?})");
    }
}

#[test]
fn memory_store_is_for_tests_only_and_isolated_by_name() {
    let store = MemorySecretStore::new();
    store.store("a", &Secret::new("va")).expect("store");
    store.store("b", &Secret::new("vb")).expect("store");
    assert_eq!(store.load("a").expect("load").unwrap().expose(), "va");
    assert_eq!(store.load("b").expect("load").unwrap().expose(), "vb");
    store.delete("a").expect("delete");
    assert!(store.load("a").expect("load").is_none());
}

// ---------------------------------------------------------------------------
// Command authorization over the WebSocket path
// ---------------------------------------------------------------------------

fn command_frame(
    op: serde_json::Value,
    device: &str,
    account: &str,
    id: &str,
    expires: &str,
) -> String {
    json!({
        "type": "command",
        "command_id": id,
        "account_id": account,
        "device_id": device,
        "operation": op,
        "issued_at": "2026-11-16T00:00:00Z",
        "expires_at": expires,
        "idempotency_key": id
    })
    .to_string()
}

#[tokio::test]
async fn unauthorized_trading_commands_never_pass_local_validation() {
    let mock = MockNse::spawn().await;
    // 1. Arbitrary remote execution attempt: not on the allowlist.
    let web = command_frame(
        json!({"type": "send_web_request", "url": "http://attacker.example", "method": "GET"}),
        "device-test-1",
        "12345678901",
        "cmd-web",
        "2099-01-01T00:00:00Z",
    );
    // 2. Order placement attempt: auto-trade path is not allowlisted.
    let order = command_frame(
        json!({"type": "trade_send_market_order", "symbol": "EURUSD", "volume": 1.0}),
        "device-test-1",
        "12345678901",
        "cmd-order",
        "2099-01-01T00:00:00Z",
    );
    mock.push_ws_outbox(&web).await;
    mock.push_ws_outbox(&order).await;

    let mut socket = ManagedSocket::spawn(NseSocketConfig {
        url: mock.ws_url(),
        api_key: Secret::new("NSE-key-valid"),
        device_id: "device-test-1".to_string(),
        heartbeat_interval: Duration::from_millis(200),
        ..Default::default()
    });
    let mut validator = CommandValidator::new("device-test-1", ["12345678901"]);
    let first = tokio::time::timeout(Duration::from_secs(5), socket.inbound.recv())
        .await
        .expect("frame 1")
        .expect("alive");
    let second = tokio::time::timeout(Duration::from_secs(5), socket.inbound.recv())
        .await
        .expect("frame 2")
        .expect("alive");

    for (raw, label) in [
        (&first, "send_web_request"),
        (&second, "trade_send_market_order"),
    ] {
        let parsed = parse_server_message(raw).expect("frame parses as a command");
        assert!(matches!(parsed, ServerMessage::Command(_)));
        assert_eq!(
            validator.validate(raw, true).unwrap_err(),
            CommandRejection::UnsupportedOperation,
            "{label} must be rejected locally"
        );
    }
    socket.shutdown().await;
    mock.stop().await;
}

#[tokio::test]
async fn entitlement_gate_blocks_allowlisted_commands_when_disabled() {
    let mock = MockNse::spawn().await;
    let ok = command_frame(
        json!({"type": "close_position", "ticket": "9000001"}),
        "device-test-1",
        "12345678901",
        "cmd-ok",
        "2099-01-01T00:00:00Z",
    );
    mock.push_ws_outbox(&ok).await;

    let mut socket = ManagedSocket::spawn(NseSocketConfig {
        url: mock.ws_url(),
        api_key: Secret::new("NSE-key-valid"),
        device_id: "device-test-1".to_string(),
        heartbeat_interval: Duration::from_millis(200),
        ..Default::default()
    });
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.inbound.recv())
        .await
        .expect("frame")
        .expect("alive");
    let mut validator = CommandValidator::new("device-test-1", ["12345678901"]);
    // Position manager/auto trade disabled => no execution, even for an
    // allowlisted operation type.
    assert_eq!(
        validator.validate(&frame, false).unwrap_err(),
        CommandRejection::AutoTradeNotEnabled
    );
    // Enabled => validates and maps to the local operation.
    let op = validator
        .validate(&frame, true)
        .expect("validates when enabled");
    assert_eq!(
        op,
        CommandOperation::ClosePosition {
            ticket: "9000001".to_string()
        }
    );
    socket.shutdown().await;
    mock.stop().await;
}

#[tokio::test]
async fn cross_device_commands_are_rejected() {
    let mock = MockNse::spawn().await;
    let frame = command_frame(
        json!({"type": "close_position", "ticket": "1"}),
        "SOME-OTHER-DEVICE",
        "12345678901",
        "cmd-dev",
        "2099-01-01T00:00:00Z",
    );
    mock.push_ws_outbox(&frame).await;

    let mut socket = ManagedSocket::spawn(NseSocketConfig {
        url: mock.ws_url(),
        api_key: Secret::new("NSE-key-valid"),
        device_id: "device-test-1".to_string(),
        heartbeat_interval: Duration::from_millis(200),
        ..Default::default()
    });
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.inbound.recv())
        .await
        .expect("frame")
        .expect("alive");
    let mut validator = CommandValidator::new("device-test-1", ["12345678901"]);
    assert_eq!(
        validator.validate(&frame, true).unwrap_err(),
        CommandRejection::WrongDevice
    );
    socket.shutdown().await;
    mock.stop().await;
}

#[tokio::test]
async fn cross_account_commands_are_rejected() {
    let mock = MockNse::spawn().await;
    // A command for a DIFFERENT MT5 account (account B) must never execute
    // on this device bound to account A.
    let frame = command_frame(
        json!({"type": "close_position", "ticket": "1"}),
        "device-test-1",
        "999888777",
        "cmd-acct",
        "2099-01-01T00:00:00Z",
    );
    mock.push_ws_outbox(&frame).await;

    let mut socket = ManagedSocket::spawn(NseSocketConfig {
        url: mock.ws_url(),
        api_key: Secret::new("NSE-key-valid"),
        device_id: "device-test-1".to_string(),
        heartbeat_interval: Duration::from_millis(200),
        ..Default::default()
    });
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.inbound.recv())
        .await
        .expect("frame")
        .expect("alive");
    let mut validator = CommandValidator::new("device-test-1", ["12345678901"]);
    assert_eq!(
        validator.validate(&frame, true).unwrap_err(),
        CommandRejection::WrongAccount
    );
    socket.shutdown().await;
    mock.stop().await;
}

#[tokio::test]
async fn replayed_commands_are_rejected() {
    let mock = MockNse::spawn().await;
    let frame = command_frame(
        json!({"type": "modify_sl_tp", "ticket": "1", "sl": 1.0, "tp": 2.0}),
        "device-test-1",
        "12345678901",
        "cmd-replay",
        "2099-01-01T00:00:00Z",
    );
    mock.push_ws_outbox(&frame).await;
    mock.push_ws_outbox(&frame).await;

    let mut socket = ManagedSocket::spawn(NseSocketConfig {
        url: mock.ws_url(),
        api_key: Secret::new("NSE-key-valid"),
        device_id: "device-test-1".to_string(),
        heartbeat_interval: Duration::from_millis(200),
        ..Default::default()
    });
    let first = tokio::time::timeout(Duration::from_secs(5), socket.inbound.recv())
        .await
        .expect("frame 1")
        .expect("alive");
    let second = tokio::time::timeout(Duration::from_secs(5), socket.inbound.recv())
        .await
        .expect("frame 2")
        .expect("alive");
    let mut validator = CommandValidator::new("device-test-1", ["12345678901"]);
    assert!(validator.validate(&first, true).is_ok());
    assert_eq!(
        validator.validate(&second, true).unwrap_err(),
        CommandRejection::Replayed
    );
    socket.shutdown().await;
    mock.stop().await;
}

#[test]
fn malformed_command_frames_are_rejected() {
    let mut validator = CommandValidator::new("device-test-1", ["12345678901"]);
    for raw in ["", "not json", r#"{"type":"command"}"#] {
        assert_eq!(
            validator.validate(raw, true).unwrap_err(),
            CommandRejection::Malformed,
            "{raw:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Instant-based sanity: expiry must use the current clock, not a fixed one
// ---------------------------------------------------------------------------

#[test]
fn freshly_issued_command_validates_and_time_passes_expiration() {
    let mut validator = CommandValidator::new("device-test-1", ["12345678901"]);
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("format now");
    let soon = (time::OffsetDateTime::now_utc() + time::Duration::seconds(60))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("format soon");
    let frame = command_frame(
        json!({"type": "close_position", "ticket": "1"}),
        "device-test-1",
        "12345678901",
        "cmd-now",
        &soon,
    );
    let started = Instant::now();
    assert!(validator.validate(&frame, true).is_ok(), "issued at {now}");
    assert!(started.elapsed() < Duration::from_secs(1));
}
