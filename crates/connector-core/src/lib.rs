//! NSE Connector core library.
//!
//! Layers:
//! - [`mcp`]: MCP streamable-HTTP client used to reach the customer's local
//!   MetaTrader 5 MCP server (handshake, bearer auth, session handling, tool
//!   discovery, tool calls).
//! - [`mt5`]: tolerant MT5 data models and mapping from MCP tool-call
//!   envelopes to typed snapshots.
//! - [`actions`]: the 106-action architecture — dynamic manifest with count
//!   validation, risk classification (Tier 0-3), universal dispatcher with
//!   local safety gates, NSE JSON contract envelopes, kill switch, and the
//!   broker symbol registry.
//! - [`nse`]: NSE Core gateway client (REST) and the WebSocket sync socket.
//!   The REST contract here is the *proposed* connector contract — the real
//!   NSE Core endpoints are the authority and must be reconciled against the
//!   deployed API before release.
//! - [`sync`]: bounded synchronization engine (batches, dedup, checkpoints,
//!   acknowledgments).
//! - [`security`]: secret storage, log redaction, and the local command
//!   validator (allowlist, expiry, replay, device/account binding).

pub mod actions;
pub mod mcp;
pub mod mt5;
pub mod nse;
pub mod security;
pub mod sync;
