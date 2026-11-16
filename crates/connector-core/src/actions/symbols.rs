//! Broker-aware symbol registry (spec §5).
//!
//! Law: brokers use different symbol names (`XAUUSDm`, `GOLD`, `EURUSD.a`,
//! `NAS100` vs `USTEC`...). The registry preserves the ORIGINAL broker
//! symbol for every MT5 call — canonical mapping is used only for analysis
//! on the server side, and only when confidently resolvable (exact match
//! after removing ONE known broker decoration). String similarity alone is
//! never enough.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// Broker symbol decorations observed in the wild (suffix OR prefix).
/// Resolution strips exactly one decoration, then requires an EXACT match
/// against the discovered symbol list.
const KNOWN_SUFFIXES: [&str; 10] = [
    "m", ".pro", ".a", ".b", ".c", ".cash", ".r", ".i", "-ecn", ".raw",
];
const KNOWN_PREFIXES: [&str; 2] = ["m.", "r."];

/// Cache TTL for discovery metadata.
pub const REGISTRY_REFRESH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SymbolEntry {
    /// The symbol exactly as the broker exposes it. This is what MT5 calls
    /// must always use.
    pub original: String,
    /// Canonical symbol when confidently resolved, else None.
    pub canonical: Option<String>,
    /// Registry discovery time (RFC 3339).
    pub discovered_at: String,
    /// Broker-reported metadata as delivered (volume min/step, digits,
    /// contract size...), when discovery supplied it.
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SymbolRegistry {
    entries: BTreeMap<String, SymbolEntry>,
}

impl SymbolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a discovered broker symbol.
    pub fn register(&mut self, original: &str, meta: serde_json::Value, discovered_at: &str) {
        self.entries.insert(
            original.to_string(),
            SymbolEntry {
                original: original.to_string(),
                canonical: None,
                discovered_at: discovered_at.to_string(),
                meta,
            },
        );
    }

    /// Resolves canonical symbols for every entry against the broker's
    /// discovered symbol list. Non-resolvable entries keep `canonical:
    /// None` — never guessed.
    pub fn resolve_canonicals(&mut self, known_broker_symbols: &[String]) {
        for entry in self.entries.values_mut() {
            entry.canonical = resolve_canonical(&entry.original, known_broker_symbols);
        }
    }

    pub fn get(&self, original: &str) -> Option<&SymbolEntry> {
        self.entries.get(original)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The symbol to use when CALLING MT5: always the original broker name.
    pub fn call_symbol<'a>(&self, original: &'a str) -> &'a str {
        original
    }
}

/// Resolves `XAUUSDm` → `XAUUSD` style mappings: strip exactly one known
/// decoration, require the remainder to EXACTLY match a discovered symbol.
/// Returns None when ambiguous or unresolvable.
pub fn resolve_canonical(original: &str, known: &[String]) -> Option<String> {
    if known.is_empty() {
        return None;
    }
    if known.iter().any(|k| k == original) {
        return Some(original.to_string());
    }
    for suffix in KNOWN_SUFFIXES {
        if let Some(base) = original.strip_suffix(suffix)
            && known.iter().any(|k| k == base)
        {
            return Some(base.to_string());
        }
    }
    for prefix in KNOWN_PREFIXES {
        if let Some(base) = original.strip_prefix(prefix)
            && known.iter().any(|k| k == base)
        {
            return Some(base.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn known() -> Vec<String> {
        ["XAUUSD", "EURUSD", "NAS100", "US30"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn exact_symbols_resolve_to_themselves() {
        assert_eq!(
            resolve_canonical("XAUUSD", &["XAUUSD".to_string()]),
            Some("XAUUSD".into())
        );
    }
    #[test]
    fn decorated_symbols_resolve_by_exact_base_match() {
        assert_eq!(
            resolve_canonical("XAUUSDm", &known()),
            Some("XAUUSD".into())
        );
        assert_eq!(
            resolve_canonical("EURUSD.a", &known()),
            Some("EURUSD".into())
        );
        assert_eq!(
            resolve_canonical("EURUSD.pro", &known()),
            Some("EURUSD".into())
        );
        assert_eq!(
            resolve_canonical("m.EURUSD", &known()),
            Some("EURUSD".into())
        );
        // Broker uses a different index name — canonical must stay None.
        assert_eq!(resolve_canonical("USTEC", &known()), None);
    }

    #[test]
    fn unknown_decorations_never_guess() {
        // Double decoration or unknown suffix => None, not a guess.
        assert_eq!(resolve_canonical("XAUUSDm.pro", &known()), None);
        assert_eq!(resolve_canonical("XAUZZZ", &known()), None);
    }

    #[test]
    fn registry_always_calls_with_the_original_symbol() {
        let mut reg = SymbolRegistry::new();
        reg.register("XAUUSDm", json!({"digits": 2}), "2026-11-17T00:00:00Z");
        assert_eq!(reg.call_symbol("XAUUSDm"), "XAUUSDm");
        reg.resolve_canonicals(&known());
        let e = reg.get("XAUUSDm").expect("entry");
        assert_eq!(e.canonical.as_deref(), Some("XAUUSD"));
    }
}
