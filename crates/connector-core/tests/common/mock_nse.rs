//! Mock NSE Core implementing the PROPOSED connector contract, with
//! configurable failure responses and full request recording (used to prove
//! the MT5 credential never leaves the machine and no secrets leak into
//! outbound traffic).
//!
//! This mock validates the contract design only. It is NOT the deployed NSE
//! Core API.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::SinkExt;
use serde_json::{Value, json};
use tokio::sync::{RwLock, watch};
use tokio::task::JoinHandle;

/// One recorded inbound HTTP request.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub path: String,
    pub authorization: Option<String>,
    pub device_id: Option<String>,
    pub body: Value,
}

#[derive(Debug, Clone)]
struct EndpointBehavior {
    status: u16,
    body: Value,
}

struct NseState {
    recorded: Mutex<Vec<RecordedRequest>>,
    validate: RwLock<Option<EndpointBehavior>>,
    register: RwLock<Option<EndpointBehavior>>,
    batch: RwLock<Option<EndpointBehavior>>,
    heartbeat: RwLock<Option<EndpointBehavior>>,
    ws_sessions: Arc<AtomicU64>,
    /// Number of inbound WS frames after which the server drops the
    /// connection (disconnect scenario).
    ws_drop_after_frames: RwLock<Option<u64>>,
    /// Frames pushed to the client on connect.
    ws_outbox: RwLock<Vec<String>>,
    ws_inbound_count: Arc<AtomicU64>,
}

pub struct MockNse {
    pub addr: SocketAddr,
    state: Arc<NseState>,
    shutdown: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

fn default_validate_body() -> Value {
    json!({
        "valid": true,
        "user_id": "user-123",
        "subscription": { "active": true, "plan": "pro", "expires_at": "2099-01-01T00:00:00Z" },
        "entitlements": {
            "history_sync": true,
            "ai_coach": true,
            "position_manager": true,
            "auto_trade": false,
            "max_devices": 3,
            "max_accounts": 2
        }
    })
}

fn error_body(code: &str) -> Value {
    json!({ "error": { "code": code, "message": "mock error" } })
}

impl MockNse {
    pub async fn spawn() -> Self {
        let state = Arc::new(NseState {
            recorded: Mutex::new(Vec::new()),
            validate: RwLock::new(None),
            register: RwLock::new(None),
            batch: RwLock::new(None),
            heartbeat: RwLock::new(None),
            ws_sessions: Arc::new(AtomicU64::new(0)),
            ws_drop_after_frames: RwLock::new(None),
            ws_outbox: RwLock::new(Vec::new()),
            ws_inbound_count: Arc::new(AtomicU64::new(0)),
        });

        let app = Router::new()
            .route("/api/v1/connector/validate-key", post(validate_handler))
            .route("/api/v1/connector/register-device", post(register_handler))
            .route("/api/v1/connector/sync/batch", post(batch_handler))
            .route("/api/v1/connector/heartbeat", post(heartbeat_handler))
            .route("/api/v1/connector/ws", get(ws_handler))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.changed().await;
                })
                .await
                .expect("mock NSE server crashed");
        });

        Self {
            addr,
            state,
            shutdown: shutdown_tx,
            handle,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn ws_url(&self) -> String {
        format!("ws://{}/api/v1/connector/ws", self.addr)
    }

    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.handle.await;
    }

    pub async fn recorded(&self) -> Vec<RecordedRequest> {
        self.state.recorded.lock().unwrap().clone()
    }

    pub async fn set_validate(&self, status: u16, code_or_body: Value) {
        *self.state.validate.write().await = Some(EndpointBehavior {
            status,
            body: code_or_body,
        });
    }

    pub async fn set_validate_error(&self, status: u16, code: &str) {
        self.set_validate(status, error_body(code)).await;
    }

    pub async fn set_register(&self, status: u16, body: Value) {
        *self.state.register.write().await = Some(EndpointBehavior { status, body });
    }

    pub async fn set_batch(&self, status: u16, body: Value) {
        *self.state.batch.write().await = Some(EndpointBehavior { status, body });
    }

    pub async fn set_heartbeat(&self, status: u16, body: Value) {
        *self.state.heartbeat.write().await = Some(EndpointBehavior { status, body });
    }

    pub async fn set_ws_drop_after_frames(&self, n: Option<u64>) {
        *self.state.ws_drop_after_frames.write().await = n;
    }

    pub async fn push_ws_outbox(&self, frame: &str) {
        self.state.ws_outbox.write().await.push(frame.to_string());
    }

    pub fn ws_sessions(&self) -> u64 {
        self.state.ws_sessions.load(Ordering::SeqCst)
    }

    pub fn ws_inbound_frames(&self) -> u64 {
        self.state.ws_inbound_count.load(Ordering::SeqCst)
    }
}

/// Shared recording + response logic.
async fn handle_rest(
    state: Arc<NseState>,
    path: &'static str,
    headers: HeaderMap,
    body: String,
    behavior: Option<EndpointBehavior>,
    default_body: Value,
) -> Response {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let device = headers
        .get("x-nse-device-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    state.recorded.lock().unwrap().push(RecordedRequest {
        path: path.to_string(),
        authorization: auth,
        device_id: device,
        body: parsed.clone(),
    });

    match behavior {
        Some(EndpointBehavior { status, body }) => {
            let sc = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (sc, Json(body)).into_response()
        }
        None => Json(default_body).into_response(),
    }
}

async fn validate_handler(
    State(state): State<Arc<NseState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let behavior = state.validate.read().await.clone();
    handle_rest(
        state,
        "/api/v1/connector/validate-key",
        headers,
        body,
        behavior,
        default_validate_body(),
    )
    .await
}

async fn register_handler(
    State(state): State<Arc<NseState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let behavior = state.register.read().await.clone();
    let default = json!({ "registered": true, "session_expires_at": "2099-01-01T00:00:00Z" });
    handle_rest(
        state,
        "/api/v1/connector/register-device",
        headers,
        body,
        behavior,
        default,
    )
    .await
}

async fn batch_handler(
    State(state): State<Arc<NseState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let behavior = state.batch.read().await.clone();
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let default = json!({
        "accepted": true,
        "batch_id": parsed.get("batch_id").cloned().unwrap_or(Value::Null)
    });
    handle_rest(
        state,
        "/api/v1/connector/sync/batch",
        headers,
        body,
        behavior,
        default,
    )
    .await
}

async fn heartbeat_handler(
    State(state): State<Arc<NseState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let behavior = state.heartbeat.read().await.clone();
    handle_rest(
        state,
        "/api/v1/connector/heartbeat",
        headers,
        body,
        behavior,
        json!({}),
    )
    .await
}

async fn ws_handler(
    State(state): State<Arc<NseState>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Record the upgrade as a request too (auth headers live here).
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let device = headers
        .get("x-nse-device-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    state.recorded.lock().unwrap().push(RecordedRequest {
        path: "/api/v1/connector/ws".to_string(),
        authorization: auth,
        device_id: device,
        body: Value::Null,
    });

    let outbox: Vec<String> = state.ws_outbox.read().await.clone();
    let drop_after: Option<u64> = *state.ws_drop_after_frames.read().await;
    let sessions = Arc::clone(&state.ws_sessions);
    let inbound = Arc::clone(&state.ws_inbound_count);

    upgrade.on_upgrade(move |socket| async move {
        handle_ws(socket, outbox, drop_after, sessions, inbound).await;
    })
}

async fn handle_ws(
    mut socket: WebSocket,
    outbox: Vec<String>,
    drop_after: Option<u64>,
    sessions: Arc<AtomicU64>,
    inbound_count: Arc<AtomicU64>,
) {
    sessions.fetch_add(1, Ordering::SeqCst);
    for frame in outbox {
        if socket.send(WsMessage::text(frame)).await.is_err() {
            return;
        }
    }
    while let Some(Ok(msg)) = socket.recv().await {
        if let WsMessage::Text(_) = &msg {
            let n = inbound_count.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(limit) = drop_after
                && n >= limit
            {
                // Abrupt drop (disconnect scenario). Close without ack.
                let _ = socket.close().await;
                return;
            }
        }
    }
}
