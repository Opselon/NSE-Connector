//! NSE gateway error taxonomy — semantic variants matching spec §8/§16.

#[derive(Debug, thiserror::Error)]
pub enum NseError {
    #[error("NSE API key invalid")]
    InvalidApiKey,

    #[error("NSE API key revoked")]
    ApiKeyRevoked,

    #[error("NSE subscription expired")]
    SubscriptionExpired,

    #[error("NSE device limit reached")]
    DeviceLimitReached,

    #[error("MT5 account not authorized by NSE Core")]
    AccountNotAuthorized,

    #[error("feature not enabled for this subscription: {0}")]
    FeatureNotEnabled(&'static str),

    #[error("rate limited by NSE Core")]
    RateLimited,

    #[error("NSE Core unavailable (HTTP {status})")]
    ServerUnavailable { status: u16 },

    #[error("NSE transport failure: {0}")]
    Transport(#[source] reqwest::Error),

    #[error("NSE request timed out")]
    Timeout,

    #[error("unexpected NSE response: {0}")]
    Malformed(String),

    #[error("NSE WebSocket failure: {0}")]
    Ws(String),
}
