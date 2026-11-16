//! NSE Core gateway client and sync socket.
//!
//! IMPORTANT (engineering spec §10): the REST contract below is the
//! *proposed* connector contract. The deployed NSE Core API is the authority;
//! every path, error code, and schema here must be reconciled against the
//! real service before release. Tests exercise this contract against a mock
//! NSE server only — mock results must never be reported as verified NSE
//! integration.

mod contract;
mod error;
mod gateway;
mod ws;

pub use contract::{
    Entitlements, NseErrorCode, RegisterDeviceRequest, RegisterDeviceResponse, SubscriptionInfo,
    SyncBatchAck, SyncBatchRequest, ValidateKeyResponse,
};
pub use error::NseError;
pub use gateway::{NseGatewayClient, NseGatewayConfig};
pub use ws::{ManagedSocket, NseSocketConfig, ServerMessage, parse_server_message};
