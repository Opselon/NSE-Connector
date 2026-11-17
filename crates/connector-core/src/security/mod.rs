//! Security primitives: secret storage, redaction, and command validation.

pub mod commands;
pub mod redact;
pub mod secrets;

pub use commands::{CommandEnvelope, CommandOperation, CommandRejection, CommandValidator};
pub use redact::{mask_login, redact_text};
pub use secrets::{
    MemorySecretStore, Secret, SecretStore, SecretStoreError, WindowsCredentialStore,
};
