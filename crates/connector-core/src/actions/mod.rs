//! 106-action support (spec §3/§5/§6/§7): dynamic manifest, risk
//! classification, universal dispatcher, JSON contract, kill switch, and
//! broker symbol registry.

pub mod contract;
pub mod dispatcher;
pub mod kill;
pub mod manifest;
pub mod risk;
pub mod schema;
pub mod symbols;

pub use contract::{
    DataQuality, MessageContext, MessageEnvelope, SyncMetadata, PROTOCOL_VERSION,
    SCHEMA_VERSION, TRANSFORMATION_VERSION,
};
pub use dispatcher::{
    ActionDispatcher, ActionRequest, ActionStatus, DispatchContext, DispatchOutcome,
    DispatchRejection,
};
pub use kill::{KillScope, KillSwitch};
pub use manifest::{ActionDescriptor, ActionManifest, CountReport, EXPECTED_ACTION_COUNT};
pub use risk::{ActionCategory, Classification, RiskTier, classify};
pub use schema::validate_against_schema;
pub use symbols::{SymbolEntry, SymbolRegistry};
