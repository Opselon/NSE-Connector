use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;

use crate::security::secrets::Secret;

use super::error::NseError;

#[derive(Debug, Clone)]
pub struct NseSocketConfig {
    /// WebSocket endpoint, `wss://` in production (spec §5D), `ws://` for the
    /// local mock during tests.
    pub url: String,
    pub api_key: Secret,
    pub device_id: String,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    pub heartbeat_interval: Duration,
}

impl Default for NseSocketConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            api_key: Secret::default(),
            device_id: String::new(),
            backoff_initial: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(15),
        }
    }
}

/// Server-to-connector messages (spec §5D/§6/§7). Anything that does not
/// parse is silently ignored — a malformed frame must not crash the sync
/// loop.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    /// Authorized command envelope (validated locally before any use — see
    /// `security::commands::CommandValidator`).
    Command(Value),
    /// Server asks for an out-of-band refresh.
    SyncRequest,
    /// Batch acknowledgment.
    Ack {
        batch_id: String,
    },
    HeartbeatOk,
}

/// Parses one server frame. Returns `None` for invalid or unknown messages.
pub fn parse_server_message(text: &str) -> Option<ServerMessage> {
    let v: Value = serde_json::from_str(text).ok()?;
    let kind = v.get("type")?.as_str()?;
    match kind {
        "command" => Some(ServerMessage::Command(v.clone())),
        "sync_request" => Some(ServerMessage::SyncRequest),
        "ack" => Some(ServerMessage::Ack {
            batch_id: v.get("batch_id")?.as_str()?.to_string(),
        }),
        "heartbeat" => Some(ServerMessage::HeartbeatOk),
        _ => None,
    }
}

/// Managed persistent connection to NSE Core with heartbeats, reconnection
/// with exponential backoff + jitter, bounded channels, and clean shutdown.
pub struct ManagedSocket {
    pub inbound: mpsc::Receiver<String>,
    pub outbound: mpsc::Sender<String>,
    sessions: Arc<AtomicU64>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl ManagedSocket {
    /// Spawns the connection supervisor task.
    pub fn spawn(cfg: NseSocketConfig) -> Self {
        let (outbound_tx, outbound_rx) = mpsc::channel::<String>(128);
        let (inbound_tx, inbound_rx) = mpsc::channel::<String>(128);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let sessions = Arc::new(AtomicU64::new(0));
        let sessions_clone = Arc::clone(&sessions);
        let task = tokio::spawn(run_loop(
            cfg,
            outbound_rx,
            inbound_tx,
            shutdown_rx,
            sessions_clone,
        ));
        Self {
            inbound: inbound_rx,
            outbound: outbound_tx,
            sessions,
            shutdown: shutdown_tx,
            task,
        }
    }

    /// Number of successful sessions established since spawn (observability
    /// + reconnect tests).
    pub fn sessions(&self) -> u64 {
        self.sessions.load(Ordering::SeqCst)
    }

    /// Signals shutdown and waits for the supervisor to exit.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

async fn run_loop(
    cfg: NseSocketConfig,
    mut outbound_rx: mpsc::Receiver<String>,
    inbound_tx: mpsc::Sender<String>,
    mut shutdown_rx: watch::Receiver<bool>,
    sessions: Arc<AtomicU64>,
) {
    let mut backoff = cfg.backoff_initial;
    loop {
        if *shutdown_rx.borrow() {
            return;
        }
        match open_session(&cfg).await {
            Ok(ws) => {
                sessions.fetch_add(1, Ordering::SeqCst);
                tracing::info!("NSE WebSocket session established");
                backoff = cfg.backoff_initial;
                let (mut sink, mut stream) = ws.split();
                let mut heartbeat = tokio::time::interval(cfg.heartbeat_interval);
                heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut clean_close = false;
                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() {
                                let _ = sink.close().await;
                                return;
                            }
                        }
                        _ = heartbeat.tick() => {
                            let frame = serde_json::json!({
                                "type": "heartbeat",
                                "device_id": cfg.device_id,
                            })
                            .to_string();
                            if sink.send(Message::text(frame)).await.is_err() {
                                break;
                            }
                        }
                        msg = outbound_rx.recv() => {
                            match msg {
                                Some(m) => {
                                    if sink.send(Message::text(m)).await.is_err() {
                                        break;
                                    }
                                }
                                None => {
                                    let _ = sink.close().await;
                                    return;
                                }
                            }
                        }
                        frame = stream.next() => {
                            match frame {
                                Some(Ok(Message::Text(t))) => {
                                    if inbound_tx.send(t.to_string()).await.is_err() {
                                        let _ = sink.close().await;
                                        return;
                                    }
                                }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => {
                                    tracing::warn!("NSE WebSocket error: {e}");
                                    break;
                                }
                                None => {
                                    clean_close = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                if clean_close {
                    tracing::info!("NSE WebSocket closed by peer; reconnecting");
                }
            }
            Err(e) => {
                tracing::warn!("NSE WebSocket connect failed: {e}");
            }
        }
        // Disconnected: backoff with jitter (spec §5D), abortable by shutdown.
        let jitter = Duration::from_millis(
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
                % 100) as u64,
        );
        let wait = (backoff + jitter).min(cfg.backoff_max);
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return;
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }
        backoff = (backoff * 2).min(cfg.backoff_max);
    }
}

async fn open_session(
    cfg: &NseSocketConfig,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    NseError,
> {
    // tungstenite 0.30 does not inject handshake headers into a custom
    // request — the client must build the complete RFC 6455 handshake.
    let uri: http::Uri = cfg
        .url
        .parse()
        .map_err(|e| NseError::Ws(format!("invalid WS URL: {e}")))?;
    if uri.scheme_str() != Some("ws") && uri.scheme_str() != Some("wss") {
        return Err(NseError::Ws(format!(
            "unsupported WS scheme: {:?}",
            uri.scheme_str()
        )));
    }
    let host_port = match uri.port_u16() {
        Some(p) => format!("{}:{}", uri.host().unwrap_or("localhost"), p),
        None => uri.host().unwrap_or("localhost").to_string(),
    };
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(uri)
        .header("Host", host_port)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        // Auth travels in the upgrade headers — never in a frame body.
        .header("Authorization", format!("Bearer {}", cfg.api_key.expose()))
        .header("X-NSE-Device-Id", &cfg.device_id)
        .body(())
        .map_err(|e| NseError::Ws(format!("failed to build WS request: {e}")))?;
    let (ws, _resp) = connect_async(request)
        .await
        .map_err(|e| NseError::Ws(format!("connect failed: {e}")))?;
    Ok(ws)
}
