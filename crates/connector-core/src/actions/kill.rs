//! Kill switch (spec §7.5): global, per-user, per-account, per-device,
//! auto-trade, and per-command-category emergency disable.
//!
//! An engaged switch immediately blocks NEW commands in its scope. In-flight
//! commands complete and are reported normally (defined policy: no partial
//! abort of already-executing MT5 operations — an interrupted trade is
//! worse than a completed one).

use std::collections::HashSet;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Which switch is active for a given command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillScope {
    Global,
    User(String),
    Account(String),
    Device(String),
    AutoTrade,
    Category(String),
}

impl KillScope {
    pub fn kind(&self) -> &'static str {
        match self {
            KillScope::Global => "global",
            KillScope::User(_) => "user",
            KillScope::Account(_) => "account",
            KillScope::Device(_) => "device",
            KillScope::AutoTrade => "auto_trade",
            KillScope::Category(_) => "category",
        }
    }
}

#[derive(Default)]
pub struct KillSwitch {
    global: AtomicBool,
    auto_trade: AtomicBool,
    users: RwLock<HashSet<String>>,
    accounts: RwLock<HashSet<String>>,
    devices: RwLock<HashSet<String>>,
    categories: RwLock<HashSet<String>>,
}

impl KillSwitch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn engage(&self, scope: &KillScope) {
        match scope {
            KillScope::Global => self.global.store(true, Ordering::SeqCst),
            KillScope::AutoTrade => self.auto_trade.store(true, Ordering::SeqCst),
            KillScope::User(u) => {
                self.users.write().expect("lock").insert(u.clone());
            }
            KillScope::Account(a) => {
                self.accounts.write().expect("lock").insert(a.clone());
            }
            KillScope::Device(d) => {
                self.devices.write().expect("lock").insert(d.clone());
            }
            KillScope::Category(c) => {
                self.categories.write().expect("lock").insert(c.clone());
            }
        }
        tracing::warn!(scope = scope.kind(), "kill switch ENGAGED");
    }

    pub fn lift(&self, scope: &KillScope) {
        match scope {
            KillScope::Global => self.global.store(false, Ordering::SeqCst),
            KillScope::AutoTrade => self.auto_trade.store(false, Ordering::SeqCst),
            KillScope::User(u) => {
                self.users.write().expect("lock").remove(u);
            }
            KillScope::Account(a) => {
                self.accounts.write().expect("lock").remove(a);
            }
            KillScope::Device(d) => {
                self.devices.write().expect("lock").remove(d);
            }
            KillScope::Category(c) => {
                self.categories.write().expect("lock").remove(c);
            }
        }
        tracing::info!(scope = scope.kind(), "kill switch lifted");
    }

    pub fn engage_all(&self) {
        self.engage(&KillScope::Global);
    }

    /// Returns the first active scope for this command context, if any.
    /// `is_financial` additionally consults the auto-trade switch.
    pub fn check(
        &self,
        user_id: Option<&str>,
        account_id: &str,
        device_id: &str,
        category: &str,
        is_financial: bool,
    ) -> Option<KillScope> {
        if self.global.load(Ordering::SeqCst) {
            return Some(KillScope::Global);
        }
        if is_financial && self.auto_trade.load(Ordering::SeqCst) {
            return Some(KillScope::AutoTrade);
        }
        if let Some(u) = user_id
            && self.users.read().expect("lock").contains(u)
        {
            return Some(KillScope::User(u.to_string()));
        }
        if self.accounts.read().expect("lock").contains(account_id) {
            return Some(KillScope::Account(account_id.to_string()));
        }
        if self.devices.read().expect("lock").contains(device_id) {
            return Some(KillScope::Device(device_id.to_string()));
        }
        if self.categories.read().expect("lock").contains(category) {
            return Some(KillScope::Category(category.to_string()));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_blocks_everything() {
        let ks = KillSwitch::new();
        assert!(ks.check(Some("u"), "a", "d", "trading", true).is_none());
        ks.engage_all();
        assert_eq!(
            ks.check(Some("u"), "a", "d", "trading", true),
            Some(KillScope::Global)
        );
        ks.lift(&KillScope::Global);
        assert!(ks.check(Some("u"), "a", "d", "trading", true).is_none());
    }

    #[test]
    fn scoped_switches_hit_only_their_scope() {
        let ks = KillSwitch::new();
        ks.engage(&KillScope::Account("12345678901".into()));
        assert_eq!(
            ks.check(Some("u1"), "12345678901", "d", "chart", false),
            Some(KillScope::Account("12345678901".into()))
        );
        assert!(ks.check(Some("u1"), "999", "d", "chart", false).is_none());
    }

    #[test]
    fn auto_trade_switch_only_gates_financial_commands() {
        let ks = KillSwitch::new();
        ks.engage(&KillScope::AutoTrade);
        assert!(ks.check(Some("u"), "a", "d", "trading", false).is_none());
        assert_eq!(
            ks.check(Some("u"), "a", "d", "trading", true),
            Some(KillScope::AutoTrade)
        );
    }

    #[test]
    fn category_switch_blocks_that_category() {
        let ks = KillSwitch::new();
        ks.engage(&KillScope::Category("files".into()));
        assert_eq!(
            ks.check(Some("u"), "a", "d", "files", false),
            Some(KillScope::Category("files".into()))
        );
        assert!(ks.check(Some("u"), "a", "d", "chart", false).is_none());
    }
}
