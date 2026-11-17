//! Action manifest (spec §3.1/§3.2): built dynamically from the real
//! server's `tools/list` — never from a hand-typed list. Contains the exact
//! tool name, description, input/output schemas, classification, and
//! permission requirements for every discovered action, with count
//! validation against the expected set.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::actions::risk::{ActionCategory, Classification, RiskTier, classify};
use crate::mcp::{McpError, McpHttpClient};

/// The action count stated by the engineering directive (136). The LIVE
/// server reported 107 actions (verified 2026-11-16/17) — the gap is
/// documented in docs/GAP_ANALYSIS.md; the directive's checklist is a
/// capability list, not the manifest. Per spec §3.2, fewer-than-expected is
/// reported, never silently accepted.
pub const EXPECTED_ACTION_COUNT: usize = 136;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDescriptor {
    /// Exact MCP tool name — this IS the action id.
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// Present only when the server exposes an `outputSchema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    pub classification: Classification,
    /// Permission string callers must hold to execute this action (derived
    /// from the classification tier; serialized as a plain string because
    /// manifests round-trip through JSON).
    pub required_permission: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionManifest {
    pub server_name: String,
    pub server_version: String,
    /// RFC 3339 discovery time.
    pub discovered_at: String,
    pub action_count: usize,
    pub actions: BTreeMap<String, ActionDescriptor>,
    /// Stable hash over the sorted (name, input schema) pairs — detects
    /// server capability changes and drives manifest versioning.
    pub content_hash: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountReport {
    pub expected: usize,
    pub actual: usize,
    pub delta: i64,
    /// `as_expected` | `more_than_expected` | `fewer_than_expected`.
    pub verdict: &'static str,
}

impl ActionManifest {
    /// Performs real discovery against the connected MCP server and builds
    /// the manifest with per-action classification.
    pub async fn discover(client: &mut McpHttpClient) -> Result<Self, McpError> {
        let handshake = client
            .handshake()
            .cloned()
            .ok_or_else(|| McpError::NotInitialized)?;
        let tools = client.list_tools().await?;
        let now = SyncEngineTime::now_rfc3339();

        let mut actions = BTreeMap::new();
        let mut hasher = Fnv1a::new();
        for tool in tools {
            hasher.update(tool.name.as_bytes());
            hasher.update(b"|");
            let input_json = tool
                .input_schema
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();
            hasher.update(input_json.as_bytes());
            let classification = classify(&tool.name, tool.description.as_deref().unwrap_or(""));
            let required_permission = classification.tier.required_permission().to_string();
            actions.insert(
                tool.name.clone(),
                ActionDescriptor {
                    name: tool.name,
                    description: tool.description.unwrap_or_default(),
                    input_schema: tool
                        .input_schema
                        .unwrap_or(serde_json::json!({ "type": "object" })),
                    output_schema: None,
                    classification,
                    required_permission,
                },
            );
        }

        Ok(Self {
            server_name: handshake.server_name,
            server_version: handshake.server_version,
            discovered_at: now,
            action_count: actions.len(),
            content_hash: hasher.finish(),
            actions,
        })
    }

    pub fn get(&self, name: &str) -> Option<&ActionDescriptor> {
        self.actions.get(name)
    }

    /// Count validation against the directive checklist (136 items) —
    /// fewer-than-expected live actions are reported, never invented.
    pub fn verify_count(&self, expected: usize) -> CountReport {
        let actual = self.action_count;
        let delta = actual as i64 - expected as i64;
        let verdict = match delta {
            0 => "as_expected",
            d if d > 0 => "more_than_expected",
            _ => "fewer_than_expected",
        };
        CountReport {
            expected,
            actual,
            delta,
            verdict,
        }
    }

    pub fn by_tier(&self, tier: RiskTier) -> Vec<&ActionDescriptor> {
        self.actions
            .values()
            .filter(|a| a.classification.tier == tier)
            .collect()
    }

    pub fn by_category(&self, category: ActionCategory) -> Vec<&ActionDescriptor> {
        self.actions
            .values()
            .filter(|a| a.classification.category == category)
            .collect()
    }

    /// `[tier0, tier1, tier2, tier3]` histogram.
    pub fn tier_histogram(&self) -> [usize; 4] {
        let mut h = [0usize; 4];
        for a in self.actions.values() {
            match a.classification.tier {
                RiskTier::ReadOnly => h[0] += 1,
                RiskTier::LocalModification => h[1] += 1,
                RiskTier::FilesystemOrCode => h[2] += 1,
                RiskTier::Financial => h[3] += 1,
            }
        }
        h
    }

    /// Difference against a previously discovered manifest: actions that
    /// disappeared (server changed / restricted installation). Per spec,
    /// missing actions are reported — never marked as supported.
    pub fn missing_since(&self, baseline: &ActionManifest) -> Vec<String> {
        baseline
            .actions
            .keys()
            .filter(|k| !self.actions.contains_key(*k))
            .cloned()
            .collect()
    }
}

/// Tiny FNV-1a (64-bit) for manifest content hashing — no crypto claims,
/// just change detection.
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn update(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Internal time helper (avoids a circular dep on sync).
struct SyncEngineTime;

impl SyncEngineTime {
    fn now_rfc3339() -> String {
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(name: &str) -> ActionDescriptor {
        let classification = classify(name, "");
        ActionDescriptor {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::json!({ "type": "object" }),
            output_schema: None,
            required_permission: classification.tier.required_permission().to_string(),
            classification,
        }
    }

    fn manifest_with(names: &[&str]) -> ActionManifest {
        let mut actions = BTreeMap::new();
        let mut hasher = Fnv1a::new();
        for n in names {
            hasher.update(n.as_bytes());
            hasher.update(b"|");
            actions.insert((*n).to_string(), descriptor(n));
        }
        ActionManifest {
            server_name: "test".into(),
            server_version: "1.0".into(),
            discovered_at: "2026-11-17T00:00:00Z".into(),
            action_count: actions.len(),
            content_hash: hasher.finish(),
            actions,
        }
    }

    #[test]
    fn count_report_covers_all_three_cases() {
        assert_eq!(
            manifest_with(&["a"; 1]).verify_count(1),
            CountReport {
                expected: 1,
                actual: 1,
                delta: 0,
                verdict: "as_expected"
            }
        );
        let more = manifest_with(&["a", "b", "c"]).verify_count(2);
        assert_eq!(more.verdict, "more_than_expected");
        assert_eq!(more.delta, 1);
        let fewer = manifest_with(&["a"]).verify_count(2);
        assert_eq!(fewer.verdict, "fewer_than_expected");
        assert_eq!(fewer.delta, -1);
    }

    #[test]
    fn manifest_hash_changes_when_capabilities_change() {
        let m = manifest_with(&["get_trading_account_info", "trade_send_market_order"]);
        let m2 = manifest_with(&["get_trading_account_info"]);
        assert_ne!(
            m.content_hash, m2.content_hash,
            "different action sets hash differently"
        );
    }

    #[test]
    fn missing_since_reports_disappeared_actions() {
        let baseline = manifest_with(&["a", "b", "c"]);
        let current = manifest_with(&["a", "c"]);
        assert_eq!(current.missing_since(&baseline), vec!["b".to_string()]);
    }
}
