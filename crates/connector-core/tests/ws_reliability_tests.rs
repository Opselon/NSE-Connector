//! WebSocket reliability suite — the 15 scenarios from the WebSocket-first
//! directive (§3), run against a real-TCP contract-faithful mock Platform
//! (axum upgrade, real frames, real disconnects — no in-process fakes).
//!
//! The command pipeline under test is the LIBRARY path
//! (`nse::session::handle_platform_frame`), i.e. the exact code the daemon
//! runs — journal dedup, gate chain, as-built `action.result` emission.
//! Timings are compressed (150ms heartbeats, 1.5s death watchdog, ≤300ms
//! backoff) so the whole suite stays deterministic and fast.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::mock_mcp::MockMcp;
use common::mock_nse::MockNse;
use connector_core::actions::dispatcher::ActionDispatcher;
use connector_core::bridge;
use connector_core::mcp::{McpClientConfig, McpHttpClient};
use connector_core::nse::session;
use connector_core::nse::{
    CommandJournal, ManagedSocket, NseGatewayClient, NseGatewayConfig, NseSocketConfig,
};
use connector_core::security::secrets::Secret;
use connector_core::sync::SyncEngine;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

const DEVICE: &str = "dev-test-1";
const LOGIN: &str = "12345678901";

/// Wires the daemon boot sequence against the mocks: REST validate/register,
/// MCP handshake + manifest discovery, persistent journal, managed socket.
struct Harness {
    nse: MockNse,
    mcp_server: MockMcp,
    socket: ManagedSocket,
    mcp: McpHttpClient,
    ctx: connector_core::actions::dispatcher::DispatchContext,
    dispatcher: ActionDispatcher,
    engine: SyncEngine,
    gateway: NseGatewayClient,
    state: std::sync::Arc<bridge::BridgeState>,
    journal: std::sync::Arc<CommandJournal>,
}

impl Harness {
    /// Boots with a fresh journal. `ws_override` replaces the mock NSE WS
    /// URL (dead-port / silent-TCP scenarios).
    async fn boot(_tag: &str, journal_path: PathBuf, ws_override: Option<String>) -> Self {
        let mcp_server = MockMcp::spawn().await;
        let nse = MockNse::spawn().await;

        let gateway = NseGatewayClient::new(NseGatewayConfig {
            base_url: nse.base_url(),
            api_key: Secret::new("nse_live_test_key_deadbeefdeadbeefdeadbeef"),
            device_id: DEVICE.to_string(),
            timeout: Duration::from_secs(10),
        })
        .expect("gateway cfg");
        let validation = gateway.validate_key().await.expect("validate");
        assert!(validation.valid);
        assert!(
            gateway
                .register_device()
                .await
                .expect("register")
                .registered
        );

        let mut mcp = McpHttpClient::new(McpClientConfig {
            endpoint: mcp_server.endpoint(),
            bearer: Some(Secret::new("test-token")),
            ..Default::default()
        })
        .expect("mcp cfg");
        mcp.connect().await.expect("mcp handshake");
        let manifest = connector_core::actions::manifest::ActionManifest::discover(&mut mcp)
            .await
            .expect("manifest discovery");

        let dispatcher = ActionDispatcher::new(
            manifest.clone(),
            std::sync::Arc::new(connector_core::actions::kill::KillSwitch::new()),
        );
        let mut ctx = connector_core::actions::dispatcher::DispatchContext::new(DEVICE);
        if let Some(u) = validation.user_id.clone() {
            ctx = ctx.with_user(u);
        }
        ctx = ctx.with_account(LOGIN);
        ctx = ctx.with_grant("local.modify");
        ctx = ctx.with_grant("advanced.execute");
        ctx = ctx.with_grant("trading.execute");
        ctx = ctx.with_financial_consent(false);

        let journal = std::sync::Arc::new(
            CommandJournal::open_with_capacity(journal_path.clone(), 10_000).expect("journal"),
        );

        let ws_url = ws_override.unwrap_or_else(|| nse.ws_url());
        let socket = ManagedSocket::spawn(NseSocketConfig {
            url: ws_url,
            api_key: Secret::new("nse_live_test_key_deadbeefdeadbeefdeadbeef"),
            device_id: DEVICE.to_string(),
            heartbeat_interval: Duration::from_millis(150),
            death_timeout: Duration::from_millis(1500),
            connect_timeout: Duration::from_secs(1),
            send_timeout: Duration::from_secs(1),
            backoff_initial: Duration::from_millis(50),
            backoff_max: Duration::from_millis(300),
        });

        // As-built manifest announcement on every session start.
        let manifest_frame = session::manifest_update_frame(&manifest);
        let _ = socket.outbound.send(manifest_frame.to_string()).await;

        Self {
            nse,
            mcp_server,
            socket,
            mcp,
            ctx,
            dispatcher,
            engine: SyncEngine::new(Default::default()),
            gateway,
            state: std::sync::Arc::new(bridge::BridgeState::new()),
            journal,
        }
    }

    /// Drives the session pipeline for `ms`, processing every inbound frame
    /// through the production path (`nse::session::handle_platform_frame`).
    async fn pump(&mut self, ms: u64) {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            let wait = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(wait, self.socket.inbound.recv()).await {
                Ok(Some(text)) => {
                    let mut deps = session::SessionDeps {
                        mcp: &mut self.mcp,
                        ctx: &mut self.ctx,
                        dispatcher: &self.dispatcher,
                        engine: &mut self.engine,
                        gateway: &self.gateway,
                        state: &self.state,
                        journal: &self.journal,
                        outbound: &self.socket.outbound,
                    };
                    session::handle_platform_frame(&text, &mut deps).await;
                }
                Ok(None) => break, // supervisor exited
                Err(_) => break,   // deadline
            }
        }
    }

    async fn shutdown(self) {
        self.socket.shutdown().await;
        self.nse.stop().await;
        self.mcp_server.stop().await;
        // NOTE: the journal file is intentionally KEPT — scenario 7 depends
        // on it surviving a restart. Tests delete it explicitly when done.
    }

    async fn action_results(&self) -> Vec<Value> {
        self.nse
            .ws_inbound_log()
            .await
            .iter()
            .filter_map(|f| serde_json::from_str::<Value>(f).ok())
            .filter(|v| v.get("type").and_then(Value::as_str) == Some("action.result"))
            .collect()
    }
}

/// As-built dialect command frame for a Tier-0 bridge command (no MCP
/// dependency — the pipeline under test is transport + gates + journal).
fn bridge_command(id: &str) -> String {
    json!({
        "type": "command",
        "command_id": id,
        "account_id": LOGIN,
        "device_id": DEVICE,
        "operation": { "type": "get_bridge_status" },
        "issued_at": "2026-11-17T00:00:00Z",
        "expires_at": "2099-01-01T00:00:00Z",
        "idempotency_key": id
    })
    .to_string()
}

fn results_for<'a>(results: &'a [Value], command_id: &str) -> Vec<&'a Value> {
    results
        .iter()
        .filter(|r| r.get("correlation_id").and_then(Value::as_str) == Some(command_id))
        .collect()
}

// ---------------------------------------------------------------------------
// 1-3: connection establishment + authentication
// ---------------------------------------------------------------------------

/// 1. Initial connection: session established, manifest announced,
///    heartbeats flowing, Platform answers them.
#[tokio::test]
async fn s01_initial_connection_establishes_and_heartbeats() {
    let mut h = Harness::boot("s01", temp_journal("s01"), None).await;
    h.pump(900).await;
    assert!(h.nse.ws_sessions() >= 1, "no WS session established");
    let inbound = h.nse.ws_inbound_log().await;
    assert!(
        inbound.iter().any(|f| f.contains("\"heartbeat\"")),
        "client must send heartbeats"
    );
    assert!(
        inbound.iter().any(|f| f.contains("manifest.update")),
        "manifest.update must be announced on connect"
    );
    h.shutdown().await;
}

/// 2. Authentication failure: server rejects pre-upgrade; the client keeps
///    retrying with backoff and connects once the server accepts again —
///    no crash, no session count inflation.
#[tokio::test]
async fn s02_auth_failure_backs_off_then_connects() {
    let nse = MockNse::spawn().await;
    nse.set_ws_auth_reject(Some(401)).await;
    let mcp_server = MockMcp::spawn().await;
    // Boot everything EXCEPT the socket, then point the socket at the
    // rejecting server.
    let journal_path = temp_journal("s02");
    let mut h = boot_with_mocks(nse, mcp_server, journal_path.clone(), None).await;
    h.pump(700).await;
    assert_eq!(h.nse.ws_sessions(), 0, "no session while auth fails");
    h.nse.set_ws_auth_reject(None).await;
    h.pump(1200).await;
    assert!(
        h.nse.ws_sessions() >= 1,
        "session established after auth clears"
    );
    h.shutdown().await;
}

/// 3. Invalid device credentials: 403 pre-upgrade recorded with the exact
///    headers the client sent — auth never rides in frames.
#[tokio::test]
async fn s03_invalid_device_credentials_rejected_pre_upgrade() {
    let nse = MockNse::spawn().await;
    nse.set_ws_auth_reject(Some(403)).await;
    let mcp_server = MockMcp::spawn().await;
    let mut h = boot_with_mocks(nse, mcp_server, temp_journal("s03"), None).await;
    h.pump(600).await;
    assert_eq!(h.nse.ws_sessions(), 0);
    let upgrades = h
        .nse
        .recorded()
        .await
        .into_iter()
        .filter(|r| r.path.contains("/ws"))
        .collect::<Vec<_>>();
    assert!(!upgrades.is_empty(), "upgrade attempts must be recorded");
    assert!(
        upgrades
            .iter()
            .all(|r| r.authorization.is_some() && r.device_id.is_some()),
        "auth must travel in the upgrade headers"
    );
    h.shutdown().await;
}

/// 4. Server unavailable: connect refused → bounded backoff → client stays
///    healthy and shuts down cleanly (never hangs).
#[tokio::test]
async fn s04_server_unavailable_backs_off_and_stays_healthy() {
    let dead = free_port();
    let mcp_server = MockMcp::spawn().await;
    let nse = MockNse::spawn().await;
    let mut h = boot_with_mocks(
        nse,
        mcp_server,
        temp_journal("s04"),
        Some(format!("ws://127.0.0.1:{dead}/api/v1/connector/ws")),
    )
    .await;
    h.pump(900).await;
    assert_eq!(h.nse.ws_sessions(), 0, "nothing to connect to");
    let t0 = Instant::now();
    h.shutdown().await;
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "shutdown must not hang"
    );
}

// ---------------------------------------------------------------------------
// 5-8: disconnects, restarts, recovery
// ---------------------------------------------------------------------------

/// 5. Network disconnection: abrupt server-side drop → automatic reconnect.
#[tokio::test]
async fn s05_network_disconnection_triggers_reconnect() {
    let mut h = Harness::boot("s05", temp_journal("s05"), None).await;
    h.nse.set_ws_drop_after_frames(Some(2)).await;
    h.pump(1500).await;
    assert!(h.nse.ws_sessions() >= 2, "must reconnect after abrupt drop");
    h.shutdown().await;
}

/// 6. Server restart with pending-command replay: the replayed command
///    executes EXACTLY once across sessions; further replays are refused
///    by the persistent journal (§4 no-duplicate-execution).
#[tokio::test]
async fn s06_server_restart_replay_executes_once_journal_refuses_rest() {
    let mut h = Harness::boot("s06", temp_journal("s06"), None).await;
    h.nse.push_ws_outbox(&bridge_command("cmd-replay-1")).await;
    h.nse.set_ws_drop_after_frames(Some(3)).await;
    h.pump(2000).await;
    let logged = h.action_results().await;
    let results = results_for(&logged, "cmd-replay-1");
    let executed = results
        .iter()
        .filter(|r| r["status"] == json!("executed"))
        .count();
    let refused = results
        .iter()
        .filter(|r| {
            r["status"] == json!("rejected")
                && r["execution_error"]
                    .as_str()
                    .is_some_and(|e| e.contains("already executed"))
        })
        .count();
    assert_eq!(executed, 1, "exactly one execution across reconnects");
    assert!(refused >= 1, "replays must be refused by the journal");
    h.shutdown().await;
}

/// 7. Client restart: a fresh connector process (same journal file) refuses
///    a replayed command that the previous process already executed.
#[tokio::test]
async fn s07_client_restart_does_not_reexecute() {
    let journal_path = temp_journal("s07");
    // First "process": executes cmd-restart-1.
    {
        let mut h = Harness::boot("s07a", journal_path.clone(), None).await;
        h.nse.push_ws_outbox(&bridge_command("cmd-restart-1")).await;
        h.pump(900).await;
        let logged = h.action_results().await;
        let results = results_for(&logged, "cmd-restart-1");
        assert_eq!(
            results
                .iter()
                .filter(|r| r["status"] == json!("executed"))
                .count(),
            1
        );
        h.shutdown().await;
    }
    // Second "process": same journal, server replays the still-pending cmd.
    {
        let mut h = Harness::boot("s07b", journal_path.clone(), None).await;
        h.nse.push_ws_outbox(&bridge_command("cmd-restart-1")).await;
        h.pump(900).await;
        let logged = h.action_results().await;
        let results = results_for(&logged, "cmd-restart-1");
        assert!(
            results.iter().all(|r| r["status"] == json!("rejected")),
            "fresh process must refuse the already-executed command"
        );
        h.shutdown().await;
    }
    let _ = std::fs::remove_file(&journal_path);
}

/// 8. Reconnection + session recovery: after reconnects the connector still
///    answers sync_request with fresh batches and the ack retires them.
#[tokio::test]
async fn s08_reconnection_recovers_session_and_syncs() {
    let mut h = Harness::boot("s08", temp_journal("s08"), None).await;
    h.nse.set_ws_drop_after_frames(Some(2)).await;
    h.nse.push_ws_outbox("{\"type\":\"sync_request\"}").await;
    h.pump(1800).await;
    assert!(h.nse.ws_sessions() >= 2);
    let batches = h
        .nse
        .recorded()
        .await
        .into_iter()
        .filter(|r| r.path.contains("sync/batch"))
        .count();
    assert!(batches >= 2, "sync must work across sessions");
    assert_eq!(h.engine.pending_count(), 0, "acks must retire batches");
    h.shutdown().await;
}

// ---------------------------------------------------------------------------
// 9-12: message handling semantics
// ---------------------------------------------------------------------------

/// 9. Duplicate messages in one session: first executes, duplicate refused.
#[tokio::test]
async fn s09_duplicate_messages_in_session_are_handled() {
    let mut h = Harness::boot("s09", temp_journal("s09"), None).await;
    h.nse.push_ws_outbox(&bridge_command("cmd-dup-1")).await;
    h.nse.push_ws_outbox(&bridge_command("cmd-dup-1")).await;
    h.pump(900).await;
    let logged = h.action_results().await;
    let results = results_for(&logged, "cmd-dup-1");
    assert_eq!(results.len(), 2, "both frames get a result");
    assert_eq!(
        results
            .iter()
            .filter(|r| r["status"] == json!("executed"))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| r["status"] == json!("rejected"))
            .count(),
        1
    );
    h.shutdown().await;
}

/// 10. Missing acknowledgment: a not-accepted batch stays queued and is
///     retried by the next sync; once the server accepts, it retires.
#[tokio::test]
async fn s10_missing_ack_retains_batch_until_accepted() {
    let mut h = Harness::boot("s10", temp_journal("s10"), None).await;
    h.nse.set_batch(200, json!({ "accepted": false })).await;
    h.nse.push_ws_outbox("{\"type\":\"sync_request\"}").await;
    h.pump(700).await;
    assert!(
        h.engine.pending_count() >= 1,
        "unacked batch must be retained"
    );
    h.nse.clear_batch().await;
    // The as-built server pushes sync_request over the LIVE socket.
    h.nse.push_live("{\"type\":\"sync_request\"}").await;
    h.pump(700).await;
    let stuck: Vec<String> = h
        .engine
        .pending()
        .map(|b| format!("{}:{}", b.kind.as_str(), b.batch_id))
        .collect();
    let sent_ids: Vec<String> = h
        .nse
        .recorded()
        .await
        .into_iter()
        .filter(|r| r.path.contains("sync/batch"))
        .filter_map(|r| {
            r.body
                .get("batch_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        h.engine.pending_count(),
        0,
        "accepted batch must retire; stuck = {stuck:?}; sent_ids = {sent_ids:?}"
    );
    h.shutdown().await;
    std::fs::remove_file(temp_journal("s10")).ok();
}

/// 11. Message ordering: commands pushed in order get results in order.
#[tokio::test]
async fn s11_message_ordering_is_preserved() {
    let mut h = Harness::boot("s11", temp_journal("s11"), None).await;
    h.nse.push_ws_outbox(&bridge_command("cmd-ord-1")).await;
    h.nse.push_ws_outbox(&bridge_command("cmd-ord-2")).await;
    h.nse.push_ws_outbox(&bridge_command("cmd-ord-3")).await;
    h.pump(900).await;
    let logged = h.action_results().await;
    let results: Vec<Value> = logged;
    let order: Vec<String> = results
        .iter()
        .filter_map(|r| {
            r.get("correlation_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let idx = |id: &str| order.iter().position(|x| x == id);
    let (a, b, c) = (idx("cmd-ord-1"), idx("cmd-ord-2"), idx("cmd-ord-3"));
    assert!(
        a.is_some() && b.is_some() && c.is_some(),
        "all three answered"
    );
    assert!(
        a.unwrap() < b.unwrap() && b.unwrap() < c.unwrap(),
        "in-order results"
    );
    h.shutdown().await;
}

/// 12. Timeout handling: a server that accepts TCP but never completes the
///     upgrade hits the bounded connect timeout — the client retries without
///     hanging.
#[tokio::test]
async fn s12_connect_timeout_when_upgrade_never_completes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        // Accept and hold — never speak WebSocket.
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 128];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
            });
        }
    });
    let mcp_server = MockMcp::spawn().await;
    let nse = MockNse::spawn().await;
    let mut h = boot_with_mocks(
        nse,
        mcp_server,
        temp_journal("s12"),
        Some(format!("ws://127.0.0.1:{port}/api/v1/connector/ws")),
    )
    .await;
    h.pump(1500).await;
    assert_eq!(h.nse.ws_sessions(), 0, "no WS session without an upgrade");
    let t0 = Instant::now();
    h.shutdown().await;
    assert!(t0.elapsed() < Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// 13-15: liveness + long-run + shutdown
// ---------------------------------------------------------------------------

/// 13. Heartbeat failure: a server that stops answering heartbeats trips the
///     inbound liveness watchdog and forces reconnects.
#[tokio::test]
async fn s13_heartbeat_failure_fires_watchdog() {
    let mut h = Harness::boot("s13", temp_journal("s13"), None).await;
    h.nse.set_ws_heartbeat_reply(false).await;
    h.pump(3500).await;
    assert!(
        h.nse.ws_sessions() >= 2,
        "silent peer must be detected by the watchdog and reconnected"
    );
    h.shutdown().await;
}

/// 14. Long-running stability: periodic drops + replays over many sessions —
///     counters stay sane, every command executes exactly once, and the
///     connector remains responsive at the end.
#[tokio::test]
async fn s14_long_running_stability_with_periodic_drops() {
    let mut h = Harness::boot("s14", temp_journal("s14"), None).await;
    h.nse.push_ws_outbox(&bridge_command("cmd-long-1")).await;
    h.nse.set_ws_drop_after_frames(Some(5)).await;
    h.pump(2500).await;
    assert!(h.nse.ws_sessions() >= 2, "reconnects must have happened");
    let logged = h.action_results().await;
    let results = results_for(&logged, "cmd-long-1");
    assert_eq!(
        results
            .iter()
            .filter(|r| r["status"] == json!("executed"))
            .count(),
        1,
        "exactly one execution despite replays"
    );
    let t0 = Instant::now();
    h.shutdown().await;
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "responsive to the end"
    );
}

/// 15. Graceful shutdown: with an active session and queued traffic,
///     shutdown completes promptly and the supervisor task joins.
#[tokio::test]
async fn s15_graceful_shutdown_completes_promptly() {
    let mut h = Harness::boot("s15", temp_journal("s15"), None).await;
    h.pump(400).await;
    assert!(h.nse.ws_sessions() >= 1);
    let t0 = Instant::now();
    h.shutdown().await;
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "graceful shutdown must be prompt, took {:?}",
        t0.elapsed()
    );
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn temp_journal(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("nse-ws-reliability");
    std::fs::create_dir_all(&dir).expect("tmp dir");
    dir.join(format!("{tag}-{}.jsonl", std::process::id()))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// Boots a Harness against ALREADY-SPAWNED mocks (auth-reject scenarios set
/// behavior before the socket exists).
async fn boot_with_mocks(
    nse: MockNse,
    mcp_server: MockMcp,
    journal_path: PathBuf,
    ws_override: Option<String>,
) -> Harness {
    let gateway = NseGatewayClient::new(NseGatewayConfig {
        base_url: nse.base_url(),
        api_key: Secret::new("nse_live_test_key_deadbeefdeadbeefdeadbeef"),
        device_id: DEVICE.to_string(),
        ..Default::default()
    })
    .expect("gateway cfg");
    let validation = gateway.validate_key().await.expect("validate");
    assert!(validation.valid);
    assert!(
        gateway
            .register_device()
            .await
            .expect("register")
            .registered
    );

    let mut mcp = McpHttpClient::new(McpClientConfig {
        endpoint: mcp_server.endpoint(),
        bearer: Some(Secret::new("test-token")),
        ..Default::default()
    })
    .expect("mcp cfg");
    mcp.connect().await.expect("mcp handshake");
    let manifest = connector_core::actions::manifest::ActionManifest::discover(&mut mcp)
        .await
        .expect("manifest discovery");

    let dispatcher = ActionDispatcher::new(
        manifest.clone(),
        std::sync::Arc::new(connector_core::actions::kill::KillSwitch::new()),
    );
    let mut ctx = connector_core::actions::dispatcher::DispatchContext::new(DEVICE);
    if let Some(u) = validation.user_id.clone() {
        ctx = ctx.with_user(u);
    }
    ctx = ctx.with_account(LOGIN);
    ctx = ctx.with_grant("local.modify");
    ctx = ctx.with_grant("advanced.execute");
    ctx = ctx.with_grant("trading.execute");
    ctx = ctx.with_financial_consent(false);

    let journal = std::sync::Arc::new(
        CommandJournal::open_with_capacity(journal_path.clone(), 10_000).expect("journal"),
    );

    let ws_url = ws_override.unwrap_or_else(|| nse.ws_url());
    let socket = ManagedSocket::spawn(NseSocketConfig {
        url: ws_url,
        api_key: Secret::new("nse_live_test_key_deadbeefdeadbeefdeadbeef"),
        device_id: DEVICE.to_string(),
        heartbeat_interval: Duration::from_millis(150),
        death_timeout: Duration::from_millis(1500),
        connect_timeout: Duration::from_secs(1),
        send_timeout: Duration::from_secs(1),
        backoff_initial: Duration::from_millis(50),
        backoff_max: Duration::from_millis(300),
    });
    let manifest_frame = session::manifest_update_frame(&manifest);
    let _ = socket.outbound.send(manifest_frame.to_string()).await;

    Harness {
        nse,
        mcp_server,
        socket,
        mcp,
        ctx,
        dispatcher,
        engine: SyncEngine::new(Default::default()),
        gateway,
        state: std::sync::Arc::new(bridge::BridgeState::new()),
        journal,
    }
}
