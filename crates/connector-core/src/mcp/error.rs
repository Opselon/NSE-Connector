use std::time::Duration;

/// Errors surfaced by the MCP client. Each variant corresponds to a failure
/// mode the Connector must handle explicitly (see engineering spec §16).
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("transport failure to MCP server: {0}")]
    Transport(#[source] reqwest::Error),

    #[error("MCP authentication failed (HTTP 401): bearer token missing or rejected")]
    Unauthorized,

    #[error("MCP session expired or unknown (HTTP 404): re-initialization required")]
    SessionExpired,

    #[error("MCP request timed out after {0:?}")]
    Timeout(Duration),

    #[error("malformed MCP response: {0}")]
    Malformed(String),

    #[error("MCP protocol mismatch: server negotiated {negotiated}, client requires {required}")]
    ProtocolMismatch {
        negotiated: String,
        required: String,
    },

    #[error("MCP tool not found: {0}")]
    ToolNotFound(String),

    #[error("MCP tool reported an error: {0}")]
    ToolFailed(String),

    #[error("MCP JSON-RPC error {code}: {message}")]
    JsonRpc { code: i64, message: String },

    #[error("MCP client is not initialized; call connect() first")]
    NotInitialized,

    #[error("MCP server is missing required tools: {0:?}")]
    MissingTools(Vec<String>),

    #[error("internal MCP client failure: {0}")]
    Internal(String),
}
