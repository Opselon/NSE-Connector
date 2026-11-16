use std::time::Duration;

use serde_json::{Value, json};

use crate::security::secrets::Secret;

use super::error::McpError;

/// The only MCP protocol version this Connector implements. The live server
/// negotiates down to its own version, so the client compares the negotiated
/// value and refuses to continue on mismatch.
pub const SUPPORTED_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone)]
pub struct McpClientConfig {
    /// Full MCP endpoint, e.g. `http://127.0.0.1:22346/mcp`.
    pub endpoint: String,
    /// Local MT5 MCP bearer token. Stays on this machine — never sent to NSE
    /// Core, never logged.
    pub bearer: Option<Secret>,
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    pub client_name: String,
    pub client_version: String,
}

impl Default for McpClientConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://127.0.0.1:22346/mcp".to_string(),
            bearer: None,
            request_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(5),
            client_name: "NSE-Connector".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HandshakeInfo {
    pub protocol_version: String,
    pub server_name: String,
    pub server_version: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
}

/// MCP streamable-HTTP client.
///
/// Session handling: after a successful `initialize` the session id from the
/// `Mcp-Session-Id` response header is echoed on every request. HTTP 404 on a
/// later call means the server no longer knows the session (restart); the
/// high-level call helpers transparently re-initialize once in that case.
pub struct McpHttpClient {
    cfg: McpClientConfig,
    http: reqwest::Client,
    session_id: Option<String>,
    handshake: Option<HandshakeInfo>,
    next_id: u64,
}

impl McpHttpClient {
    pub fn new(cfg: McpClientConfig) -> Result<Self, McpError> {
        let url = reqwest::Url::parse(&cfg.endpoint)
            .map_err(|e| McpError::Malformed(format!("invalid MCP endpoint URL: {e}")))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(McpError::Malformed(format!(
                "unsupported MCP endpoint scheme: {}",
                url.scheme()
            )));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(cfg.connect_timeout)
            .timeout(cfg.request_timeout)
            .build()
            .map_err(|e| McpError::Internal(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            cfg,
            http,
            session_id: None,
            handshake: None,
            next_id: 0,
        })
    }

    pub fn handshake(&self) -> Option<&HandshakeInfo> {
        self.handshake.as_ref()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Overrides the session id (session restoration from a checkpoint, or
    /// forcing a stale-session scenario). Handshake state is preserved: a
    /// stale session surfaces as `McpError::SessionExpired` on the next
    /// strict call, and `call_tool_raw` recovers by re-initializing.
    pub fn restore_session(&mut self, session_id: Option<String>) {
        self.session_id = session_id;
    }

    fn next_request_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Single POST to the MCP endpoint with auth/session/protocol headers.
    async fn post(&self, payload: &Value) -> Result<(u16, Option<String>, String), McpError> {
        let mut req = self
            .http
            .post(&self.cfg.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", SUPPORTED_PROTOCOL_VERSION);
        if let Some(token) = &self.cfg.bearer {
            req = req.header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", token.expose()),
            );
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let resp = req
            .json(payload)
            .send()
            .await
            .map_err(|e| map_transport(e, self.cfg.request_timeout))?;
        let status = resp.status().as_u16();
        let session = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = resp
            .text()
            .await
            .map_err(|e| map_transport(e, self.cfg.request_timeout))?;
        Ok((status, session, body))
    }

    /// Performs the `initialize` handshake + `notifications/initialized`.
    pub async fn connect(&mut self) -> Result<HandshakeInfo, McpError> {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": SUPPORTED_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": self.cfg.client_name,
                    "version": self.cfg.client_version,
                }
            }
        });
        let (status, session, body) = self.post(&payload).await?;
        match status {
            401 => return Err(McpError::Unauthorized),
            200 => {}
            other => {
                return Err(McpError::Malformed(format!(
                    "initialize returned unexpected HTTP status {other}"
                )));
            }
        }
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| McpError::Malformed(format!("initialize body: {e}")))?;
        if let Some(err) = parsed.get("error") {
            return Err(McpError::JsonRpc {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        let result = parsed
            .get("result")
            .ok_or_else(|| McpError::Malformed("initialize response missing result".into()))?;
        let negotiated = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::Malformed("initialize result missing protocolVersion".into()))?
            .to_string();
        if negotiated != SUPPORTED_PROTOCOL_VERSION {
            return Err(McpError::ProtocolMismatch {
                negotiated,
                required: SUPPORTED_PROTOCOL_VERSION.to_string(),
            });
        }
        let info = HandshakeInfo {
            protocol_version: negotiated,
            server_name: result
                .pointer("/serverInfo/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            server_version: result
                .pointer("/serverInfo/version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            session_id: session,
        };
        tracing::info!(
            server = %info.server_name,
            version = %info.server_version,
            protocol = %info.protocol_version,
            "MCP handshake complete"
        );

        // Complete the handshake with the initialized notification (202 on
        // the live server; any 2xx/202-style response is acceptable).
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        self.session_id = info.session_id.clone();
        let (notif_status, _, _) = self.post(&notification).await?;
        if notif_status >= 400 {
            return Err(McpError::Malformed(format!(
                "notifications/initialized returned HTTP {notif_status}"
            )));
        }
        self.handshake = Some(info.clone());
        Ok(info)
    }

    /// Ensures a session exists before issuing calls.
    pub async fn ensure_connected(&mut self) -> Result<(), McpError> {
        if self.handshake.is_none() {
            self.connect().await?;
        }
        Ok(())
    }

    /// Tool discovery (`tools/list`).
    pub async fn list_tools(&mut self) -> Result<Vec<ToolDescriptor>, McpError> {
        self.ensure_connected().await?;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "tools/list"
        });
        let (status, _, body) = self.post(&payload).await?;
        match status {
            401 => return Err(McpError::Unauthorized),
            404 => return Err(McpError::SessionExpired),
            200 => {}
            other => {
                return Err(McpError::Malformed(format!(
                    "tools/list returned unexpected HTTP status {other}"
                )));
            }
        }
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| McpError::Malformed(format!("tools/list body: {e}")))?;
        if let Some(err) = parsed.get("error") {
            return Err(McpError::JsonRpc {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        let tools = parsed
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::Malformed("tools/list result missing tools array".into()))?;
        Ok(tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name").and_then(Value::as_str)?.to_string();
                Some(ToolDescriptor {
                    name,
                    description: t
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    input_schema: t.get("inputSchema").cloned(),
                })
            })
            .collect())
    }

    /// Verifies every required tool is present in a discovery result.
    pub fn verify_required_tools(
        tools: &[ToolDescriptor],
        required: &[&str],
    ) -> Result<(), McpError> {
        let missing: Vec<String> = required
            .iter()
            .filter(|r| !tools.iter().any(|t| t.name == **r))
            .map(|r| (*r).to_string())
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(McpError::MissingTools(missing))
        }
    }

    /// High-level tool call: extracts the JSON payload from the tool result
    /// envelope (JSON-in-text) and transparently re-initializes once when the
    /// server reports the session unknown (HTTP 404).
    pub async fn call_tool_json(&mut self, name: &str, args: &Value) -> Result<Value, McpError> {
        let envelope = self.call_tool_raw(name, args).await?;
        extract_tool_json(&envelope)
    }

    /// Tool call returning the raw JSON-RPC `result` envelope, with one
    /// session-recovery retry on HTTP 404.
    pub async fn call_tool_raw(&mut self, name: &str, args: &Value) -> Result<Value, McpError> {
        self.ensure_connected().await?;
        match self.call_tool_raw_once(name, args).await {
            Err(McpError::SessionExpired) => {
                tracing::info!("MCP session expired; re-initializing once");
                self.connect().await?;
                self.call_tool_raw_once(name, args).await
            }
            other => other,
        }
    }

    /// Tool call without session recovery — exposes the raw HTTP 404 /
    /// session-expired behavior for diagnostics and tests.
    pub async fn call_tool_raw_once(
        &mut self,
        name: &str,
        args: &Value,
    ) -> Result<Value, McpError> {
        if self.handshake.is_none() {
            return Err(McpError::NotInitialized);
        }
        let payload = json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        let (status, _, body) = self.post(&payload).await?;
        match status {
            401 => return Err(McpError::Unauthorized),
            404 => return Err(McpError::SessionExpired),
            200 => {}
            other => {
                return Err(McpError::Malformed(format!(
                    "tools/call returned unexpected HTTP status {other}"
                )));
            }
        }
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| McpError::Malformed(format!("tools/call body: {e}")))?;
        if let Some(err) = parsed.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // Live server behavior: unknown tool => HTTP 200 + -32602.
            if code == -32602 && message.contains("not found") {
                return Err(McpError::ToolNotFound(name.to_string()));
            }
            return Err(McpError::JsonRpc { code, message });
        }
        parsed
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::Malformed("tools/call response missing result".into()))
    }
}

/// Extracts the tool payload from the JSON-RPC result envelope.
///
/// The MT5 MCP server returns the payload as a JSON string inside
/// `result.content[0].text` with `result.isError: false`. This function
/// verifies `isError`, picks the first text content, and parses the embedded
/// JSON.
pub fn extract_tool_json(envelope: &Value) -> Result<Value, McpError> {
    if envelope.get("isError").and_then(Value::as_bool) == Some(true) {
        let text = envelope
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("tool reported isError without details");
        return Err(McpError::ToolFailed(text.to_string()));
    }
    let content = envelope
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::Malformed("tool result missing content array".into()))?;
    let text = content
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("text"))
        .and_then(|c| c.get("text").and_then(Value::as_str))
        .ok_or_else(|| McpError::Malformed("tool result contains no text content".into()))?;
    serde_json::from_str(text)
        .map_err(|e| McpError::Malformed(format!("tool text content is not valid JSON: {e}")))
}

fn map_transport(e: reqwest::Error, configured_timeout: Duration) -> McpError {
    if e.is_timeout() {
        McpError::Timeout(configured_timeout)
    } else {
        McpError::Transport(e)
    }
}
