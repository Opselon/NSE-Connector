//! Mock MCP server reproducing the LIVE server's wire behavior
//! (`MetaTrader 5 MCP` v1.0, protocol 2025-06-18) with injectable failure
//! scenarios for controlled testing (spec §16: mocks are clearly
//! distinguished from real MT5 integration tests).
//!
//! Reproduced live behaviors:
//! - Missing/wrong bearer => 401 + `WWW-Authenticate: Bearer`
//! - `initialize` => 200 + `Mcp-Session-Id` header
//! - `notifications/initialized` => 202
//! - Unknown session => 404 (empty body)
//! - Unknown tool => HTTP 200 + JSON-RPC `-32602 "tool not found"`
//! - Tool payload delivered as JSON string inside `result.content[0].text`

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::{RwLock, watch};
use tokio::task::JoinHandle;

/// What a tool call should do.
#[derive(Clone)]
pub enum ToolBehavior {
    /// Normal success: payload serialized into `content[0].text` (JSON-in-text).
    TextJson(Value),
    /// Arbitrary result envelope (e.g. missing content => malformed path).
    RawResult(Value),
    /// `content[0].text` present but NOT valid JSON.
    TextNotJson(&'static str),
    /// LIVE-VERIFIED behavior (E100): mutation tools answer with plain text
    /// and isError=false — a text success, not a malformed response.
    TextPlain(&'static str),
    /// `result.isError: true` with a message.
    IsError(String),
    /// Sleep, then answer (timeout scenario).
    Hang(Duration),
    /// JSON-RPC error (e.g. `-32602 "tool not found"`).
    JsonRpcError(i64, String),
}

struct McpState {
    require_auth: RwLock<bool>,
    valid_token: RwLock<String>,
    protocol_version: RwLock<String>,
    sessions: RwLock<HashSet<String>>,
    tools: RwLock<Value>,
    tool_behavior: RwLock<HashMap<String, ToolBehavior>>,
    malformed_json: RwLock<bool>,
    session_counter: RwLock<u64>,
}

/// Running mock server handle.
pub struct MockMcp {
    pub addr: SocketAddr,
    state: Arc<McpState>,
    shutdown: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl MockMcp {
    /// Spawns a mock with the four trading tools registered and sane
    /// defaults (auth required, token `test-token`).
    pub async fn spawn() -> Self {
        let state = Arc::new(McpState {
            require_auth: RwLock::new(true),
            valid_token: RwLock::new("test-token".to_string()),
            protocol_version: RwLock::new(
                connector_core::mcp::SUPPORTED_PROTOCOL_VERSION.to_string(),
            ),
            sessions: RwLock::new(HashSet::new()),
            tools: RwLock::new(json!([
                trading_tool(connector_core::mcp::trading_tools::ACCOUNT_INFO),
                trading_tool(connector_core::mcp::trading_tools::OPEN_POSITIONS),
                trading_tool(connector_core::mcp::trading_tools::HISTORY_ORDERS),
                trading_tool(connector_core::mcp::trading_tools::HISTORY_POSITIONS),
            ])),
            tool_behavior: RwLock::new(HashMap::new()),
            malformed_json: RwLock::new(false),
            session_counter: RwLock::new(0),
        });

        let app = Router::new()
            .route("/mcp", post(mcp_handler))
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
                .expect("mock MCP server crashed");
        });

        Self {
            addr,
            state,
            shutdown: shutdown_tx,
            handle,
        }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.handle.await;
    }

    pub async fn set_require_auth(&self, required: bool) {
        *self.state.require_auth.write().await = required;
    }

    pub async fn set_valid_token(&self, token: &str) {
        *self.state.valid_token.write().await = token.to_string();
    }

    pub async fn set_protocol_version(&self, version: &str) {
        *self.state.protocol_version.write().await = version.to_string();
    }

    pub async fn set_tools(&self, tools: Value) {
        *self.state.tools.write().await = tools;
    }

    pub async fn set_tool_behavior(&self, tool: &str, behavior: ToolBehavior) {
        self.state
            .tool_behavior
            .write()
            .await
            .insert(tool.to_string(), behavior);
    }

    pub async fn clear_tool_behavior(&self, tool: &str) {
        self.state.tool_behavior.write().await.remove(tool);
    }

    /// Simulates a server restart from the session's point of view: every
    /// previously issued session becomes unknown (404) — exactly what the
    /// live server does after a restart.
    pub async fn invalidate_sessions(&self) {
        self.state.sessions.write().await.clear();
    }

    pub async fn set_malformed_json(&self, on: bool) {
        *self.state.malformed_json.write().await = on;
    }

    pub async fn session_count(&self) -> usize {
        self.state.sessions.read().await.len()
    }
}

fn trading_tool(name: &str) -> Value {
    json!({
        "name": name,
        "description": format!("Mock of {name}. Read-only."),
        "inputSchema": { "type": "object", "properties": {}, "required": [] }
    })
}

fn rpc_error(code: i64, message: &str, id: Option<Value>) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        })),
    )
        .into_response()
}

async fn mcp_handler(
    State(state): State<Arc<McpState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Malformed-JSON scenario: answer with garbage.
    if *state.malformed_json.read().await {
        return (StatusCode::OK, "{{{ this is not json").into_response();
    }

    // Bearer auth (live behavior: 401 + WWW-Authenticate).
    if *state.require_auth.read().await {
        let expected = state.valid_token.read().await.clone();
        let provided = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));
        if provided != Some(expected.as_str()) {
            return (
                StatusCode::UNAUTHORIZED,
                [(
                    axum::http::header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Bearer realm=\"MetaTrader5-MCP\""),
                )],
                "",
            )
                .into_response();
        }
    }

    let parsed: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return rpc_error(-32700, "parse error", None),
    };
    let id = parsed.get("id").cloned();
    let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");

    match method {
        "initialize" => {
            let mut counter = state.session_counter.write().await;
            *counter += 1;
            let sid = format!("MOCK-SESSION-{:08X}", *counter);
            drop(counter);
            state.sessions.write().await.insert(sid.clone());
            let version = state.protocol_version.read().await.clone();
            let mut resp = Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "MockMT5-MCP", "version": "1.0" }
                }
            }))
            .into_response();
            if let Ok(hv) = HeaderValue::from_str(&sid) {
                resp.headers_mut().insert("Mcp-Session-Id", hv);
            }
            resp.headers_mut().insert(
                "MCP-Protocol-Version",
                HeaderValue::from_static("2025-06-18"),
            );
            resp
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" | "tools/call" => {
            // Session enforcement (live behavior: unknown session => 404,
            // empty body).
            let sid = headers
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !state.sessions.read().await.contains(sid) {
                return (StatusCode::NOT_FOUND, "").into_response();
            }
            if method == "tools/list" {
                let tools = state.tools.read().await.clone();
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "tools": tools }
                }))
                .into_response()
            } else {
                let name = parsed
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // Live behavior: a tool that does not exist on the server
                // returns HTTP 200 + JSON-RPC -32602 "tool not found",
                // regardless of any injected behavior.
                let known = state
                    .tools
                    .read()
                    .await
                    .as_array()
                    .map(|tools| {
                        tools
                            .iter()
                            .any(|t| t.get("name").and_then(Value::as_str) == Some(name.as_str()))
                    })
                    .unwrap_or(false);
                if !known {
                    return rpc_error(-32602, "tool not found", id);
                }
                let behavior = state.tool_behavior.read().await.get(&name).cloned();
                match behavior {
                    Some(ToolBehavior::JsonRpcError(code, msg)) => rpc_error(code, &msg, id),
                    Some(ToolBehavior::Hang(d)) => {
                        tokio::time::sleep(d).await;
                        envelope_result(id, json!({ "note": "late answer" }))
                    }
                    Some(ToolBehavior::IsError(msg)) => Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "isError": true,
                            "content": [ { "type": "text", "text": msg } ]
                        }
                    }))
                    .into_response(),
                    Some(ToolBehavior::TextPlain(text)) => Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "isError": false,
                            "content": [ { "type": "text", "text": text } ]
                        }
                    }))
                    .into_response(),
                    Some(ToolBehavior::TextNotJson(text)) => Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "isError": false,
                            "content": [ { "type": "text", "text": text } ]
                        }
                    }))
                    .into_response(),
                    Some(ToolBehavior::RawResult(v)) => Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": v
                    }))
                    .into_response(),
                    Some(ToolBehavior::TextJson(payload)) => envelope_result(id, payload),
                    None => envelope_result(id, json!({})),
                }
            }
        }
        _ => rpc_error(-32601, "method not found", id),
    }
}

fn envelope_result(id: Option<Value>, payload: Value) -> Response {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "isError": false,
            "content": [ { "type": "text", "text": payload.to_string() } ]
        }
    }))
    .into_response()
}
