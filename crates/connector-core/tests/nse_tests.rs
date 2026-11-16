//! NSE tests (engineering spec §16 "NSE Tests") — every bullet mapped to a
//! named test, executed against the mock NSE Core implementing the PROPOSED
//! connector contract.
//!
//! HONESTY NOTE: these validate the contract design only. The deployed NSE
//! Core API has not been verified in this session; before release every
//! endpoint/error code must be reconciled against the real service.

mod common;

use std::time::Duration;

use common::mock_nse::MockNse;
use connector_core::nse::NseErrorCode;
use connector_core::nse::{
    ManagedSocket, NseError, NseGatewayClient, NseGatewayConfig, NseSocketConfig, ServerMessage,
    parse_server_message,
};
use connector_core::security::commands::{CommandRejection, CommandValidator};
use connector_core::security::secrets::Secret;
use serde_json::json;
use std::time::Instant;

fn gateway(mock: &MockNse, key: &str) -> NseGatewayClient {
    NseGatewayClient::new(NseGatewayConfig {
        base_url: mock.base_url(),
        api_key: Secret::new(key),
        device_id: "device-test-1".to_string(),
        timeout: Duration::from_secs(5),
    })
    .expect("gateway configuration is valid")
}

// ---------------------------------------------------------------------------
// API key validation matrix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn valid_api_key_is_accepted_with_entitlements() {
    let mock = MockNse::spawn().await;
    let resp = gateway(&mock, "NSE-key-valid")
        .validate_key()
        .await
        .expect("valid key");
    assert!(resp.valid);
    assert_eq!(resp.user_id.as_deref(), Some("user-123"));
    let ent = &resp.entitlements;
    assert!(ent.history_sync && ent.ai_coach && ent.position_manager);
    assert!(!ent.auto_trade, "auto trade must stay off by default");
    assert_eq!(ent.max_devices, 3);
    mock.stop().await;
}

#[tokio::test]
async fn invalid_api_key_is_rejected() {
    let mock = MockNse::spawn().await;
    mock.set_validate_error(401, "key_invalid").await;
    let err = gateway(&mock, "NSE-key-bad")
        .validate_key()
        .await
        .expect_err("must reject");
    assert!(matches!(err, NseError::InvalidApiKey), "got: {err:?}");
    mock.stop().await;
}

#[tokio::test]
async fn revoked_api_key_is_rejected() {
    let mock = MockNse::spawn().await;
    mock.set_validate_error(401, "key_revoked").await;
    let err = gateway(&mock, "NSE-key-revoked")
        .validate_key()
        .await
        .expect_err("must reject");
    assert!(matches!(err, NseError::ApiKeyRevoked), "got: {err:?}");
    mock.stop().await;
}

#[tokio::test]
async fn expired_subscription_is_detected() {
    let mock = MockNse::spawn().await;
    mock.set_validate_error(403, "subscription_expired").await;
    let err = gateway(&mock, "NSE-key-old")
        .validate_key()
        .await
        .expect_err("must reject");
    assert!(matches!(err, NseError::SubscriptionExpired), "got: {err:?}");
    mock.stop().await;
}

#[tokio::test]
async fn device_limit_exceeded_is_detected() {
    let mock = MockNse::spawn().await;
    mock.set_validate_error(403, "device_limit_exceeded").await;
    let err = gateway(&mock, "NSE-key-limit")
        .validate_key()
        .await
        .expect_err("must reject");
    assert!(matches!(err, NseError::DeviceLimitReached), "got: {err:?}");
    mock.stop().await;
}

#[tokio::test]
async fn unauthorized_account_is_detected() {
    let mock = MockNse::spawn().await;
    mock.set_validate_error(403, "account_not_authorized").await;
    let err = gateway(&mock, "NSE-key-ok")
        .validate_key()
        .await
        .expect_err("must reject");
    assert!(
        matches!(err, NseError::AccountNotAuthorized),
        "got: {err:?}"
    );
    mock.stop().await;
}

#[tokio::test]
async fn feature_not_enabled_is_detected() {
    let mock = MockNse::spawn().await;
    mock.set_validate_error(403, "feature_not_enabled").await;
    let err = gateway(&mock, "NSE-key-ok")
        .validate_key()
        .await
        .expect_err("must reject");
    assert!(
        matches!(err, NseError::FeatureNotEnabled(_)),
        "got: {err:?}"
    );
    mock.stop().await;
}

#[tokio::test]
async fn server_unavailable_maps_to_retryable_error() {
    let mock = MockNse::spawn().await;
    mock.set_validate(503, json!({ "error": { "code": "internal" } }))
        .await;
    let err = gateway(&mock, "NSE-key-ok")
        .validate_key()
        .await
        .expect_err("must fail");
    assert!(
        matches!(err, NseError::ServerUnavailable { status: 503 }),
        "got: {err:?}"
    );
    mock.stop().await;
}

#[tokio::test]
async fn rate_limited_is_detected() {
    let mock = MockNse::spawn().await;
    mock.set_validate(429, json!({ "error": { "code": "rate_limited" } }))
        .await;
    let err = gateway(&mock, "NSE-key-ok")
        .validate_key()
        .await
        .expect_err("must fail");
    assert!(matches!(err, NseError::RateLimited), "got: {err:?}");
    mock.stop().await;
}

#[tokio::test]
async fn unknown_error_code_maps_to_malformed_not_silent_success() {
    let mock = MockNse::spawn().await;
    mock.set_validate_error(403, "some_future_code").await;
    let err = gateway(&mock, "NSE-key-ok")
        .validate_key()
        .await
        .expect_err("must not pass");
    assert!(matches!(err, NseError::Malformed(_)), "got: {err:?}");
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Device registration + heartbeat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn device_registration_and_heartbeat_succeed() {
    let mock = MockNse::spawn().await;
    let g = gateway(&mock, "NSE-key-valid");
    let reg = g.register_device().await.expect("register");
    assert!(reg.registered);
    g.heartbeat().await.expect("heartbeat");
    let recorded = mock.recorded().await;
    assert!(
        recorded
            .iter()
            .any(|r| r.path == "/api/v1/connector/register-device")
    );
    assert!(
        recorded
            .iter()
            .any(|r| r.path == "/api/v1/connector/heartbeat")
    );
    // Key travels in the Authorization header only.
    assert!(
        recorded
            .iter()
            .all(|r| r.authorization.as_deref() == Some("Bearer NSE-key-valid"))
    );
    assert!(
        recorded
            .iter()
            .all(|r| !r.body.to_string().contains("NSE-key-valid"))
    );
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// WebSocket: connect, receive, reconnect, invalid messages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ws_connects_receives_and_parses_server_frames() {
    let mock = MockNse::spawn().await;
    mock.push_ws_outbox(r#"{"type":"command","command_id":"c1"}"#)
        .await;
    mock.push_ws_outbox("totally not json").await; // invalid frame tolerated

    let mut socket = ManagedSocket::spawn(NseSocketConfig {
        url: mock.ws_url(),
        api_key: Secret::new("NSE-key-valid"),
        device_id: "device-test-1".to_string(),
        backoff_initial: Duration::from_millis(100),
        heartbeat_interval: Duration::from_millis(200),
        ..Default::default()
    });

    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while received.len() < 2 && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), socket.inbound.recv()).await {
            Ok(Some(frame)) => received.push(frame),
            Ok(None) => panic!("socket closed unexpectedly"),
            Err(_) => break, // timeout loop tick
        }
    }
    assert_eq!(received.len(), 2, "both frames must arrive: {received:?}");
    // Valid command frame parses; garbage frame is ignored (None).
    let parsed = parse_server_message(&received[0]).expect("command frame must parse");
    assert!(
        matches!(parsed, ServerMessage::Command(v) if v.get("command_id") == Some(&json!("c1")))
    );
    assert_eq!(
        parse_server_message(&received[1]),
        None,
        "garbage must be ignored"
    );
    // The socket must still be healthy afterwards.
    assert_eq!(socket.sessions(), 1);
    socket
        .outbound
        .send(json!({"type":"ack","batch_id":"b1"}).to_string())
        .await
        .expect("send works");
    socket.shutdown().await;
    mock.stop().await;
}

#[tokio::test]
async fn ws_reconnects_after_disconnect_with_backoff() {
    let mock = MockNse::spawn().await;
    // Server drops the connection right after the first client frame.
    mock.set_ws_drop_after_frames(Some(1)).await;

    let socket = ManagedSocket::spawn(NseSocketConfig {
        url: mock.ws_url(),
        api_key: Secret::new("NSE-key-valid"),
        device_id: "device-test-1".to_string(),
        backoff_initial: Duration::from_millis(150),
        heartbeat_interval: Duration::from_millis(150),
        ..Default::default()
    });

    // Heartbeats flow => server drops => client reconnects (>= 2 sessions).
    let deadline = Instant::now() + Duration::from_secs(10);
    while socket.sessions() < 2 {
        assert!(Instant::now() < deadline, "no reconnect within 10s");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        mock.ws_sessions() >= 2,
        "server must have seen the second session"
    );
    socket.shutdown().await;
    mock.stop().await;
}

#[tokio::test]
async fn ws_auth_headers_are_sent_on_upgrade() {
    let mock = MockNse::spawn().await;
    let socket = ManagedSocket::spawn(NseSocketConfig {
        url: mock.ws_url(),
        api_key: Secret::new("NSE-key-valid"),
        device_id: "device-test-1".to_string(),
        heartbeat_interval: Duration::from_millis(200),
        ..Default::default()
    });
    // Wait for session establishment.
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.sessions() < 1 {
        assert!(Instant::now() < deadline, "no WS session within 5s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let recorded = mock.recorded().await;
    let ws = recorded
        .iter()
        .find(|r| r.path == "/api/v1/connector/ws")
        .expect("ws upgrade recorded");
    assert_eq!(ws.authorization.as_deref(), Some("Bearer NSE-key-valid"));
    assert_eq!(ws.device_id.as_deref(), Some("device-test-1"));
    socket.shutdown().await;
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Command expiration + duplicates over the wire (spec §16 NSE list)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expired_command_over_ws_is_rejected_locally() {
    let mock = MockNse::spawn().await;
    let frame = json!({
        "type": "command",
        "command_id": "cmd-exp-1",
        "account_id": "12345678901",
        "device_id": "device-test-1",
        "operation": { "type": "close_position", "ticket": "9000001" },
        "issued_at": "2026-11-16T00:00:00Z",
        "expires_at": "2026-11-16T01:00:00Z",
        "idempotency_key": "cmd-exp-1"
    });
    mock.push_ws_outbox(&frame.to_string()).await;

    let mut socket = ManagedSocket::spawn(NseSocketConfig {
        url: mock.ws_url(),
        api_key: Secret::new("NSE-key-valid"),
        device_id: "device-test-1".to_string(),
        heartbeat_interval: Duration::from_millis(200),
        ..Default::default()
    });
    let frame_in = tokio::time::timeout(Duration::from_secs(5), socket.inbound.recv())
        .await
        .expect("frame within 5s")
        .expect("socket alive");
    let parsed = parse_server_message(&frame_in).expect("parses");
    let ServerMessage::Command(_) = &parsed else {
        panic!("expected command, got {parsed:?}");
    };
    let mut validator = CommandValidator::new("device-test-1", ["12345678901"]);
    assert_eq!(
        validator.validate(&frame_in, true).unwrap_err(),
        CommandRejection::Expired,
        "expired command must never reach MT5"
    );
    socket.shutdown().await;
    mock.stop().await;
}

#[tokio::test]
async fn duplicate_command_over_ws_is_rejected_as_replay() {
    let mock = MockNse::spawn().await;
    let frame = json!({
        "type": "command",
        "command_id": "cmd-dup-1",
        "account_id": "12345678901",
        "device_id": "device-test-1",
        "operation": { "type": "modify_sl_tp", "ticket": "9000001", "sl": 1.083, "tp": 1.087 },
        "issued_at": "2026-11-16T00:00:00Z",
        "expires_at": "2099-01-01T00:00:00Z",
        "idempotency_key": "cmd-dup-1"
    });
    mock.push_ws_outbox(&frame.to_string()).await;
    mock.push_ws_outbox(&frame.to_string()).await; // duplicate delivery

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
        .expect("first frame")
        .expect("alive");
    let second = tokio::time::timeout(Duration::from_secs(5), socket.inbound.recv())
        .await
        .expect("second frame")
        .expect("alive");

    assert!(
        validator.validate(&first, true).is_ok(),
        "first delivery validates"
    );
    assert_eq!(
        validator.validate(&second, true).unwrap_err(),
        CommandRejection::Replayed,
        "duplicate must be rejected"
    );
    socket.shutdown().await;
    mock.stop().await;
}

// ---------------------------------------------------------------------------
// Error-code parsing contract
// ---------------------------------------------------------------------------

#[test]
fn error_codes_parse_to_the_documented_variants() {
    for (code, expect) in [
        ("key_invalid", NseErrorCode::KeyInvalid),
        ("key_revoked", NseErrorCode::KeyRevoked),
        ("subscription_expired", NseErrorCode::SubscriptionExpired),
        ("device_limit_exceeded", NseErrorCode::DeviceLimitExceeded),
        ("account_not_authorized", NseErrorCode::AccountNotAuthorized),
        ("feature_not_enabled", NseErrorCode::FeatureNotEnabled),
        ("rate_limited", NseErrorCode::RateLimited),
    ] {
        assert_eq!(NseErrorCode::parse(code), expect, "{code}");
    }
    assert_eq!(NseErrorCode::parse("nonsense"), NseErrorCode::Unknown);
}
