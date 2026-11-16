//! Proposed NSE Connector REST contract (schema v0).
//!
//! Status: PROPOSED — pending reconciliation with the deployed NSE Core API.
//! The local MT5 MCP credential must NEVER appear anywhere in these
//! structures.

use serde::{Deserialize, Serialize};

/// `POST /api/v1/connector/validate-key`
///
/// The API key travels only in the `Authorization: Bearer` header — never in
/// the body, query string, or logs.
#[derive(Debug, Clone, Serialize)]
pub struct ValidateKeyRequest {
    pub device_id: String,
    pub connector_version: String,
}

/// Server-owned entitlement decisions (spec §8: NSE Core is authoritative;
/// the Connector never hardcodes plans or quotas).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ValidateKeyResponse {
    pub valid: bool,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub subscription: Option<SubscriptionInfo>,
    #[serde(default)]
    pub entitlements: Entitlements,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SubscriptionInfo {
    pub active: bool,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Secure default: every capability disabled until the server says otherwise
/// (`false` / `0` are exactly what the derived `Default` produces).
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct Entitlements {
    pub history_sync: bool,
    pub ai_coach: bool,
    pub position_manager: bool,
    pub auto_trade: bool,
    pub max_devices: u32,
    pub max_accounts: u32,
}

/// `POST /api/v1/connector/register-device`
#[derive(Debug, Clone, Serialize)]
pub struct RegisterDeviceRequest {
    pub device_id: String,
    pub connector_version: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RegisterDeviceResponse {
    pub registered: bool,
    #[serde(default)]
    pub session_expires_at: Option<String>,
}

/// `POST /api/v1/connector/sync/batch`
///
/// One bounded batch per synchronization step. `items` carries the raw MT5
/// payload (account snapshot / positions / history page) with broker
/// timestamps preserved verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct SyncBatchRequest {
    pub batch_id: String,
    /// `account` | `positions` | `history_orders` | `history_positions`
    pub kind: String,
    pub items: serde_json::Value,
    pub count: u32,
    /// Connector-side production time (RFC 3339).
    pub produced_at: String,
}

/// `POST /api/v1/connector/sync/batch` response.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SyncBatchAck {
    pub accepted: bool,
    #[serde(default)]
    pub batch_id: Option<String>,
}

/// Machine-readable error codes proposed for the NSE API. HTTP mapping:
/// 401 => `key_invalid` / `key_revoked`; 403 => the authorization variants;
/// 429 => `rate_limited`; 5xx => transport-level unavailability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NseErrorCode {
    KeyInvalid,
    KeyRevoked,
    SubscriptionExpired,
    DeviceLimitExceeded,
    AccountNotAuthorized,
    FeatureNotEnabled,
    RateLimited,
    Unknown,
}

impl NseErrorCode {
    /// Parses the proposed `error.code` string from an NSE error body.
    pub fn parse(code: &str) -> Self {
        match code {
            "key_invalid" => Self::KeyInvalid,
            "key_revoked" => Self::KeyRevoked,
            "subscription_expired" => Self::SubscriptionExpired,
            "device_limit_exceeded" => Self::DeviceLimitExceeded,
            "account_not_authorized" => Self::AccountNotAuthorized,
            "feature_not_enabled" => Self::FeatureNotEnabled,
            "rate_limited" => Self::RateLimited,
            _ => Self::Unknown,
        }
    }
}
