//! NSE Core gateway client and sync socket.
//!
//! IMPORTANT (engineering spec §10): the REST contract below is the
//! *proposed* connector contract. The deployed NSE Core API is the authority;
//! every path, error code, and schema here must be reconciled against the
//! real service before release. Tests exercise this contract against a mock
//! NSE server only — mock results must never be reported as verified NSE
//! integration.

mod command_frame;
mod command_journal;
mod contract;
mod error;
mod gateway;
pub mod session;
mod ws;

/// Protocol version the connector reports in `action.result` /
/// `manifest.update` frames. The as-built Platform validates it against
/// `"{PROTOCOL_VERSION}.0"` (server `mod.rs`), i.e. `"1.0"`.
pub const WS_PROTOCOL_VERSION: &str = "1.0";

pub use command_frame::IncomingCommand;
pub use command_journal::CommandJournal;
pub use contract::{
    Entitlements, NseErrorCode, RegisterDeviceRequest, RegisterDeviceResponse, SubscriptionInfo,
    SyncBatchAck, SyncBatchRequest, ValidateKeyResponse,
};
pub use error::NseError;
pub use gateway::{NseGatewayClient, NseGatewayConfig};
pub use ws::{ManagedSocket, NseSocketConfig, ServerMessage, parse_server_message};
