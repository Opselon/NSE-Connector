//! Secret handling: an opaque wrapper that redacts itself in `Debug`, plus
//! storage behind the [`SecretStore`] port (Windows Credential Manager in
//! production, memory implementation for tests).

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use thiserror::Error;

/// A credential value that never reveals itself in `Debug`/`Display` output
/// or derive-based logging.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Explicit accessor — the naming (`expose`) makes accidental logging in
    /// code review obvious.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Typed error for the [`SecretStore`] port — matchable variants instead of
/// bare `String`, so callers keep a proper error chain.
#[derive(Debug, Error)]
pub enum SecretStoreError {
    /// The platform credential backend failed (I/O, access denied, ...).
    #[error("platform credential store failure: {0}")]
    Platform(String),
}

/// Port for durable credential storage. Production uses Windows Credential
/// Manager; tests use [`MemorySecretStore`].
pub trait SecretStore: Send + Sync {
    fn store(&self, name: &str, value: &Secret) -> Result<(), SecretStoreError>;
    fn load(&self, name: &str) -> Result<Option<Secret>, SecretStoreError>;
    fn delete(&self, name: &str) -> Result<(), SecretStoreError>;
}

/// In-memory implementation for tests only — never persisted to disk.
#[derive(Default)]
pub struct MemorySecretStore {
    map: Mutex<HashMap<String, Secret>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for MemorySecretStore {
    fn store(&self, name: &str, value: &Secret) -> Result<(), SecretStoreError> {
        self.map
            .lock()
            .map_err(|e| SecretStoreError::Platform(e.to_string()))?
            .insert(name.to_string(), value.clone());
        Ok(())
    }

    fn load(&self, name: &str) -> Result<Option<Secret>, SecretStoreError> {
        Ok(self
            .map
            .lock()
            .map_err(|e| SecretStoreError::Platform(e.to_string()))?
            .get(name)
            .cloned())
    }

    fn delete(&self, name: &str) -> Result<(), SecretStoreError> {
        self.map
            .lock()
            .map_err(|e| SecretStoreError::Platform(e.to_string()))?
            .remove(name);
        Ok(())
    }
}

/// Windows Credential Manager backed store (via the `keyring` crate).
///
/// Credentials live in the OS credential vault — never in plaintext files,
/// config, or logs.
pub struct WindowsCredentialStore {
    service: String,
}

impl WindowsCredentialStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, name: &str) -> Result<keyring::Entry, SecretStoreError> {
        keyring::Entry::new(&self.service, name)
            .map_err(|e| SecretStoreError::Platform(e.to_string()))
    }
}

impl SecretStore for WindowsCredentialStore {
    fn store(&self, name: &str, value: &Secret) -> Result<(), SecretStoreError> {
        self.entry(name)?
            .set_password(value.expose())
            .map_err(|e| SecretStoreError::Platform(e.to_string()))
    }

    fn load(&self, name: &str) -> Result<Option<Secret>, SecretStoreError> {
        match self.entry(name)?.get_password() {
            Ok(pw) => Ok(Some(Secret::new(pw))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretStoreError::Platform(e.to_string())),
        }
    }

    fn delete(&self, name: &str) -> Result<(), SecretStoreError> {
        match self.entry(name)?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretStoreError::Platform(e.to_string())),
        }
    }
}
