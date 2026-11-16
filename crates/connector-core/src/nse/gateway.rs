use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::security::secrets::Secret;

use super::contract::{
    NseErrorCode, RegisterDeviceRequest, RegisterDeviceResponse, SyncBatchAck, SyncBatchRequest,
    ValidateKeyRequest, ValidateKeyResponse,
};
use super::error::NseError;

#[derive(Debug, Clone)]
pub struct NseGatewayConfig {
    /// NSE Core base URL, e.g. `https://bridge.nexusverse.space`.
    pub base_url: String,
    /// NSE API key generated from the customer's website account. Stored via
    /// [`crate::security::SecretStore`], never logged, never sent to MT5.
    pub api_key: Secret,
    pub device_id: String,
    pub timeout: Duration,
}

impl Default for NseGatewayConfig {
    fn default() -> Self {
        Self {
            base_url: "https://bridge.nexusverse.space".to_string(),
            api_key: Secret::default(),
            device_id: String::new(),
            timeout: Duration::from_secs(15),
        }
    }
}

/// REST client for NSE Core connector endpoints.
///
/// The API key is only ever sent as an `Authorization: Bearer` header.
#[derive(Clone)]
pub struct NseGatewayClient {
    cfg: NseGatewayConfig,
    http: reqwest::Client,
}

impl NseGatewayClient {
    pub fn new(cfg: NseGatewayConfig) -> Result<Self, NseError> {
        let url = reqwest::Url::parse(&cfg.base_url)
            .map_err(|e| NseError::Malformed(format!("invalid NSE base URL: {e}")))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(NseError::Malformed(format!(
                "unsupported NSE base URL scheme: {}",
                url.scheme()
            )));
        }
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| NseError::Malformed(format!("failed to build HTTP client: {e}")))?;
        Ok(Self { cfg, http })
    }

    pub fn config(&self) -> &NseGatewayConfig {
        &self.cfg
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path)
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, NseError> {
        let resp = self
            .http
            .post(self.url(path))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.cfg.api_key.expose()),
            )
            .header("X-NSE-Device-Id", &self.cfg.device_id)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    NseError::Timeout
                } else {
                    NseError::Transport(e)
                }
            })?;
        map_response(resp).await
    }

    /// Validates the configured API key + device against NSE Core.
    pub async fn validate_key(&self) -> Result<ValidateKeyResponse, NseError> {
        let body = serde_json::to_value(ValidateKeyRequest {
            device_id: self.cfg.device_id.clone(),
            connector_version: env!("CARGO_PKG_VERSION").to_string(),
        })
        .map_err(|e| NseError::Malformed(e.to_string()))?;
        self.post("/api/v1/connector/validate-key", &body).await
    }

    /// Registers (or re-registers) this device identity.
    pub async fn register_device(&self) -> Result<RegisterDeviceResponse, NseError> {
        let body = serde_json::to_value(RegisterDeviceRequest {
            device_id: self.cfg.device_id.clone(),
            connector_version: env!("CARGO_PKG_VERSION").to_string(),
        })
        .map_err(|e| NseError::Malformed(e.to_string()))?;
        self.post("/api/v1/connector/register-device", &body).await
    }

    /// Sends one synchronization batch and returns the acknowledgment.
    pub async fn send_batch(&self, batch: &SyncBatchRequest) -> Result<SyncBatchAck, NseError> {
        let body = serde_json::to_value(batch).map_err(|e| NseError::Malformed(e.to_string()))?;
        self.post("/api/v1/connector/sync/batch", &body).await
    }

    /// Keep-alive heartbeat for a registered device session.
    pub async fn heartbeat(&self) -> Result<(), NseError> {
        self.post::<serde_json::Value>("/api/v1/connector/heartbeat", &serde_json::json!({}))
            .await
            .map(|_| ())
    }
}

/// Maps an NSE response to the semantic error taxonomy (spec §16 NSE tests).
async fn map_response<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, NseError> {
    let status = resp.status().as_u16();
    let body = resp.text().await.map_err(NseError::Transport)?;
    match status {
        200 => serde_json::from_str(&body)
            .map_err(|e| NseError::Malformed(format!("status 200 body: {e}"))),
        401 => Err(code_error(&body, 401)),
        403 => Err(code_error(&body, 403)),
        429 => Err(NseError::RateLimited),
        status if status >= 500 => Err(NseError::ServerUnavailable { status }),
        other => Err(NseError::Malformed(format!(
            "unexpected HTTP status {other}: {}",
            truncate(&body, 160)
        ))),
    }
}

fn code_error(body: &str, status: u16) -> NseError {
    let parsed: Result<Value, _> = serde_json::from_str(body);
    let code = parsed
        .ok()
        .and_then(|v| {
            v.pointer("/error/code")
                .or_else(|| v.get("code"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    match NseErrorCode::parse(&code) {
        NseErrorCode::KeyInvalid => NseError::InvalidApiKey,
        NseErrorCode::KeyRevoked => NseError::ApiKeyRevoked,
        NseErrorCode::SubscriptionExpired => NseError::SubscriptionExpired,
        NseErrorCode::DeviceLimitExceeded => NseError::DeviceLimitReached,
        NseErrorCode::AccountNotAuthorized => NseError::AccountNotAuthorized,
        NseErrorCode::FeatureNotEnabled => {
            NseError::FeatureNotEnabled("requested feature not entitled")
        }
        NseErrorCode::RateLimited => NseError::RateLimited,
        NseErrorCode::Unknown => NseError::Malformed(format!(
            "HTTP {status} without a known NSE error code: {}",
            truncate(body, 160)
        )),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}
