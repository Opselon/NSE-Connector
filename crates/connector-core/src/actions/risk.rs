//! Risk classification for MCP actions (spec §7.3).
//!
//! The final classification is based on actual tool names, descriptions,
//! and observed behavior — never on a name being harmless by itself.
//! Unknown registered actions default to Tier 2 (fail-closed): they stay
//! callable only with the advanced-execution permission.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Execution risk tiers (spec §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RiskTier {
    /// Tier 0 — read only (account info, history, charts, journals).
    ReadOnly,
    /// Tier 1 — local non-financial modification (charts, indicators,
    /// Market Watch, alerts, profiles).
    LocalModification,
    /// Tier 2 — filesystem/code/network/service operations (file writes and
    /// deletes, script/EA attachment, external HTTP, services, tester runs).
    FilesystemOrCode,
    /// Tier 3 — direct financial impact (order placement, position close,
    /// SL/TP modification, order deletion).
    Financial,
}

impl RiskTier {
    /// Default permission string a caller must hold for this tier.
    pub fn required_permission(&self) -> &'static str {
        match self {
            RiskTier::ReadOnly => "",
            RiskTier::LocalModification => "local.modify",
            RiskTier::FilesystemOrCode => "advanced.execute",
            RiskTier::Financial => "trading.execute",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RiskTier::ReadOnly => "tier0_read_only",
            RiskTier::LocalModification => "tier1_local_modify",
            RiskTier::FilesystemOrCode => "tier2_fs_code",
            RiskTier::Financial => "tier3_financial",
        }
    }
}

/// Functional category of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActionCategory {
    Trading,
    Account,
    MarketData,
    Chart,
    Files,
    Terminal,
    Calendar,
    News,
    Alerts,
    Tester,
    Workspace,
    Network,
    Service,
    Other,
}

impl ActionCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionCategory::Trading => "trading",
            ActionCategory::Account => "account",
            ActionCategory::MarketData => "market_data",
            ActionCategory::Chart => "chart",
            ActionCategory::Files => "files",
            ActionCategory::Terminal => "terminal",
            ActionCategory::Calendar => "calendar",
            ActionCategory::News => "news",
            ActionCategory::Alerts => "alerts",
            ActionCategory::Tester => "tester",
            ActionCategory::Workspace => "workspace",
            ActionCategory::Network => "network",
            ActionCategory::Service => "service",
            ActionCategory::Other => "other",
        }
    }
}

/// Classification result with the justification (spec: the classification
/// must be documented per action).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub tier: RiskTier,
    pub category: ActionCategory,
    pub reason: String,
}

/// Explicit overrides for actions whose names alone are misleading, or whose
/// behavior was verified to be sensitive (spec §7.3: "Do not assume a tool
/// is harmless based only on its name" — e.g. a chart template can attach an
/// EA and indirectly start trading).
fn overrides() -> &'static HashMap<&'static str, Classification> {
    static MAP: OnceLock<HashMap<&'static str, Classification>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        let mut put =
            |name: &'static str, tier: RiskTier, cat: ActionCategory, reason: &'static str| {
                m.insert(
                    name,
                    Classification {
                        tier,
                        category: cat,
                        reason: reason.to_string(),
                    },
                );
            };
        use ActionCategory as C;
        use RiskTier as T;

        // Tier 3 — verified financial execution paths.
        put(
            "trade_send_market_order",
            T::Financial,
            C::Trading,
            "places a market order",
        );
        put(
            "trade_send_pending_order",
            T::Financial,
            C::Trading,
            "places a pending order",
        );
        put(
            "trade_close_single_position",
            T::Financial,
            C::Trading,
            "closes a position",
        );
        put(
            "trade_close_by_position",
            T::Financial,
            C::Trading,
            "closes positions by opposite position",
        );
        put(
            "trade_modify_sl_tp",
            T::Financial,
            C::Trading,
            "modifies stop loss / take profit",
        );
        put(
            "trade_delete_order",
            T::Financial,
            C::Trading,
            "deletes (cancels) a pending order",
        );

        // Tier 0 — trading-adjacent but purely computational (verified
        // read-only by description: "never places, modifies or cancels").
        put(
            "trade_check_market_order",
            T::ReadOnly,
            C::Trading,
            "validation calculation only",
        );
        put(
            "trade_check_pending_order",
            T::ReadOnly,
            C::Trading,
            "validation calculation only",
        );
        put(
            "trade_order_calc_margin",
            T::ReadOnly,
            C::Trading,
            "margin calculation only",
        );
        put(
            "trade_order_calc_profit",
            T::ReadOnly,
            C::Trading,
            "profit calculation only",
        );

        // Tier 2 — network, filesystem, code, services.
        put(
            "send_web_request",
            T::FilesystemOrCode,
            C::Network,
            "arbitrary outbound HTTP from the terminal",
        );
        put(
            "write_file",
            T::FilesystemOrCode,
            C::Files,
            "writes terminal files",
        );
        put(
            "write_binary_file",
            T::FilesystemOrCode,
            C::Files,
            "writes binary terminal files",
        );
        put(
            "create_new_file",
            T::FilesystemOrCode,
            C::Files,
            "creates files",
        );
        put(
            "create_new_folder",
            T::FilesystemOrCode,
            C::Files,
            "creates folders",
        );
        put(
            "delete_file",
            T::FilesystemOrCode,
            C::Files,
            "deletes files",
        );
        put(
            "delete_folder",
            T::FilesystemOrCode,
            C::Files,
            "deletes folders",
        );
        put(
            "replace_text_in_file",
            T::FilesystemOrCode,
            C::Files,
            "modifies file contents",
        );
        put(
            "chart_add_expert",
            T::FilesystemOrCode,
            C::Chart,
            "attaches an EA — can initiate trading",
        );
        put(
            "chart_add_script",
            T::FilesystemOrCode,
            C::Chart,
            "runs a script on a chart",
        );
        put(
            "chart_apply_template",
            T::FilesystemOrCode,
            C::Chart,
            "template may embed an EA",
        );
        put(
            "service_register",
            T::FilesystemOrCode,
            C::Service,
            "registers a terminal service program",
        );
        put(
            "service_start",
            T::FilesystemOrCode,
            C::Service,
            "starts a service program",
        );
        put(
            "service_stop",
            T::FilesystemOrCode,
            C::Service,
            "stops a service program",
        );
        put(
            "service_unregister",
            T::FilesystemOrCode,
            C::Service,
            "unregisters a service program",
        );
        put(
            "tester_run_backtest",
            T::FilesystemOrCode,
            C::Tester,
            "executes MQL programs in the tester",
        );
        put(
            "tester_run_optimization",
            T::FilesystemOrCode,
            C::Tester,
            "executes MQL programs in the tester",
        );
        put(
            "tester_prepare_config",
            T::FilesystemOrCode,
            C::Tester,
            "writes tester configuration files",
        );
        put(
            "tester_prepare_inputs",
            T::FilesystemOrCode,
            C::Tester,
            "writes tester input files",
        );

        // Tier 1 — local non-financial modifications.
        for name in [
            "chart_open",
            "chart_close",
            "chart_navigate",
            "chart_set_properties",
            "chart_set_symbol_period",
            "chart_add_indicator",
            "chart_remove_indicator",
            "chart_remove_expert",
            "chart_object_create",
            "chart_object_update",
            "chart_object_delete",
            "chart_save_template",
            "chart_delete_template",
            "add_marketwatch_symbol",
            "remove_marketwatch_symbol",
            "market_book_subscribe",
            "market_book_unsubscribe",
            "profile_add",
            "profile_apply",
            "profile_delete",
            "alerts_create",
            "alerts_update",
            "alerts_delete",
            "alerts_set_enabled",
            "tester_stop",
            "tester_close_result_window",
        ] {
            put(
                name,
                T::LocalModification,
                C::Chart,
                "modifies local terminal UI state",
            );
        }
        m
    })
}

/// Read-only name prefixes: any registered action starting with one of these
/// and not overridden is treated as Tier 0.
const READ_PREFIXES: [&str; 8] = [
    "get_",
    "list_",
    "read_",
    "search_",
    "find_",
    "economic_calendar_",
    "news_",
    "market_book_get",
];

/// Classifies an action by exact name (overrides first), then read-prefix
/// heuristic, then fail-closed Tier 2.
pub fn classify(name: &str, _description: &str) -> Classification {
    if let Some(c) = overrides().get(name) {
        return c.clone();
    }
    if READ_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Classification {
            tier: RiskTier::ReadOnly,
            category: infer_category(name),
            reason: "read-only by verified name prefix (get/list/read/search/find/calendar/news)"
                .to_string(),
        };
    }
    // Fail-closed: registered but unclassified => treat as advanced.
    Classification {
        tier: RiskTier::FilesystemOrCode,
        category: infer_category(name),
        reason: "unclassified action defaults to Tier 2 (fail-closed)".to_string(),
    }
}

fn infer_category(name: &str) -> ActionCategory {
    use ActionCategory as C;
    if name.starts_with("trade_") {
        C::Trading
    } else if name.starts_with("chart_") {
        C::Chart
    } else if name.contains("file") || name.contains("folder") || name.contains("directory") {
        C::Files
    } else if name.starts_with("market_")
        || name.contains("marketwatch")
        || name.starts_with("get_chart")
    {
        C::MarketData
    } else if name.starts_with("economic_calendar_") {
        C::Calendar
    } else if name.contains("news") {
        C::News
    } else if name.starts_with("alerts") {
        C::Alerts
    } else if name.starts_with("tester_") {
        C::Tester
    } else if name.contains("service") {
        C::Service
    } else if name.starts_with("get_trading") || name.contains("terminal") {
        C::Terminal
    } else if name.contains("workspace") {
        C::Workspace
    } else {
        C::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_financial_tools_are_tier3() {
        for name in [
            "trade_send_market_order",
            "trade_send_pending_order",
            "trade_close_single_position",
            "trade_close_by_position",
            "trade_modify_sl_tp",
            "trade_delete_order",
        ] {
            let c = classify(name, "");
            assert_eq!(c.tier, RiskTier::Financial, "{name}");
            assert_eq!(c.category, ActionCategory::Trading);
        }
    }

    #[test]
    fn trading_calculations_are_read_only() {
        assert_eq!(
            classify("trade_order_calc_profit", "").tier,
            RiskTier::ReadOnly
        );
        assert_eq!(
            classify("trade_check_market_order", "").tier,
            RiskTier::ReadOnly
        );
    }

    #[test]
    fn dangerous_names_are_not_trusted_as_harmless() {
        // send_web_request is Tier 2 despite sounding like a utility.
        assert_eq!(
            classify("send_web_request", "").tier,
            RiskTier::FilesystemOrCode
        );
        // A chart template can attach an EA and start trading.
        assert_eq!(
            classify("chart_apply_template", "").tier,
            RiskTier::FilesystemOrCode
        );
        assert_eq!(
            classify("chart_add_expert", "").tier,
            RiskTier::FilesystemOrCode
        );
    }

    #[test]
    fn read_prefixes_are_tier0() {
        for name in [
            "get_trading_account_info",
            "get_trading_open_positions",
            "get_trading_history_orders",
            "get_terminal_journal",
            "list_directory",
            "read_file",
            "search_text",
            "economic_calendar_list_events_by_country",
            "get_news_list",
            "market_book_get",
        ] {
            assert_eq!(
                classify(name, "reads data").tier,
                RiskTier::ReadOnly,
                "{name}"
            );
        }
    }

    #[test]
    fn unknown_actions_fail_closed_to_tier2() {
        let c = classify("brand_new_future_action", "");
        assert_eq!(c.tier, RiskTier::FilesystemOrCode);
        assert!(c.reason.contains("fail-closed"));
    }

    #[test]
    fn local_ui_mutations_are_tier1() {
        for name in [
            "chart_open",
            "chart_navigate",
            "add_marketwatch_symbol",
            "alerts_create",
            "profile_apply",
        ] {
            assert_eq!(
                classify(name, "").tier,
                RiskTier::LocalModification,
                "{name}"
            );
        }
    }
}
