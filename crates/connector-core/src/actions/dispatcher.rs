//! Universal MCP action dispatcher (spec §3.3 / §7).
//!
//! Execution gate chain — every step is enforced LOCALLY, regardless of what
//! the server claims:
//! 1. Parse the request envelope.
//! 2. Kill switch (immediate block, any scope).
//! 3. Expiration.
//! 4. Device binding.
//! 5. Account binding (cross-account isolation).
//! 6. Manifest resolution — unknown actions are never executed.
//! 7. Parameter validation against the action's input JSON Schema.
//! 8. Permission gate: Tier 0 needs nothing; Tier 1/2/3 need their tier
//!    permission in the server-granted set; Tier 3 additionally needs the
//!    customer's explicit financial consent AND the auto-trade entitlement.
//! 9. Replay protection (bounded idempotency cache) — only successful
//!    validations consume an id.
//! 10. Real MCP execution with the verified result returned to the server.
//!
//! A delivered command is NEVER treated as an executed trade; results carry
//! the actual MCP payload.

use std::collections::HashSet;
use std::sync::RwLock;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::actions::kill::{KillScope, KillSwitch};
use crate::actions::manifest::ActionManifest;
use crate::actions::risk::RiskTier;
use crate::actions::schema::validate_against_schema;
use crate::mcp::{McpError, McpHttpClient, ToolOutcome, extract_tool_result};
use crate::security::redact::mask_login;

/// Server-to-connector action request (spec §6.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRequest {
    pub request_id: String,
    pub command_id: String,
    pub action_name: String,
    pub account_id: String,
    pub device_id: String,
    /// RFC 3339.
    pub issued_at: String,
    /// RFC 3339.
    pub expires_at: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub parameters: Value,
    /// Server authorization context (resolved user identity, grants, ...).
    #[serde(default)]
    pub authorization_context: Value,
}

/// Execution status reported back (spec §6.3 distinguishes delivery from
/// execution).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionStatus {
    /// Locally rejected — never executed.
    Rejected,
    /// MCP tool ran and returned a payload.
    Executed,
    /// Authorized but the MCP call itself failed.
    McpFailed,
}

impl ActionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionStatus::Rejected => "rejected",
            ActionStatus::Executed => "executed",
            ActionStatus::McpFailed => "mcp_failed",
        }
    }
}

/// Why a command was rejected locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchRejection {
    Malformed,
    Expired,
    Replayed,
    WrongDevice,
    WrongAccount,
    UnknownAction(String),
    SchemaValidation(Vec<String>),
    PermissionDenied { tier: RiskTier, required: String },
    FinancialConsentMissing,
    KillSwitchActive(KillScope),
    PayloadTooLarge { actual: usize, limit: usize },
}

impl std::fmt::Display for DispatchRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchRejection::Malformed => f.write_str("command envelope malformed"),
            DispatchRejection::Expired => f.write_str("command expired"),
            DispatchRejection::Replayed => f.write_str("command replayed"),
            DispatchRejection::WrongDevice => f.write_str("command bound to another device"),
            DispatchRejection::WrongAccount => f.write_str("command bound to another account"),
            DispatchRejection::UnknownAction(name) => {
                write!(f, "action not in the current manifest: {name}")
            }
            DispatchRejection::SchemaValidation(errors) => {
                write!(f, "parameter validation failed: {}", errors.join("; "))
            }
            DispatchRejection::PermissionDenied { tier, required } => {
                write!(f, "tier {tier:?} requires permission '{required}'")
            }
            DispatchRejection::FinancialConsentMissing => {
                f.write_str("financial execution requires explicit customer consent")
            }
            DispatchRejection::KillSwitchActive(scope) => {
                write!(f, "kill switch active ({})", scope.kind())
            }
            DispatchRejection::PayloadTooLarge { actual, limit } => {
                write!(f, "parameters too large ({actual} > {limit} bytes)")
            }
        }
    }
}

/// Result reported back to NSE Core (spec §6.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchOutcome {
    pub request_id: String,
    pub command_id: String,
    pub action_name: String,
    pub status: ActionStatus,
    pub started_at: String,
    pub completed_at: String,
    pub duration_ms: u64,
    /// The actual MCP tool payload (extracted from the JSON-in-text
    /// envelope), when executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_mcp_result: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_error: Option<String>,
    pub correlation_id: String,
}

/// Local authorization context the Connector derives from its OWN validated
/// state — never trusted from the request frame alone.
#[derive(Debug, Clone)]
pub struct DispatchContext {
    /// This device's registered id.
    pub device_id: String,
    /// MT5 logins authorized for this device (server-validated at login).
    pub authorized_accounts: HashSet<String>,
    /// Server-resolved user id for kill-switch scoping.
    pub user_id: Option<String>,
    /// Permission strings granted by NSE Core (e.g. "local.modify",
    /// "advanced.execute", "trading.execute").
    pub grants: HashSet<String>,
    /// Explicit customer consent for financial execution on this account.
    pub financial_consent: bool,
}

impl DispatchContext {
    pub fn new(device_id: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            authorized_accounts: HashSet::new(),
            user_id: None,
            grants: HashSet::new(),
            financial_consent: false,
        }
    }

    pub fn with_account(mut self, login: impl Into<String>) -> Self {
        self.authorized_accounts.insert(login.into());
        self
    }

    pub fn with_grant(mut self, permission: impl Into<String>) -> Self {
        self.grants.insert(permission.into());
        self
    }

    pub fn with_user(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    pub fn with_financial_consent(mut self, consent: bool) -> Self {
        self.financial_consent = consent;
        self
    }
}

/// Bounded replay cache (same policy as the Tier-3 command validator).
struct BoundedIdSet {
    capacity: usize,
    seen: HashSet<String>,
    order: VecDeque<String>,
}

use std::collections::VecDeque;

impl BoundedIdSet {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, id: &str) -> bool {
        if self.seen.contains(id) {
            return false;
        }
        while self.seen.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.seen.insert(id.to_string());
        self.order.push_back(id.to_string());
        true
    }
}

const MAX_PARAM_BYTES: usize = 16 * 1024;

/// The universal dispatcher. Holds the current manifest (refreshable when
/// the server's capabilities change) and the kill switch.
pub struct ActionDispatcher {
    manifest: RwLock<Arc<ActionManifest>>,
    kill: Arc<KillSwitch>,
    replay: Mutex<BoundedIdSet>,
}

impl ActionDispatcher {
    pub fn new(manifest: ActionManifest, kill: Arc<KillSwitch>) -> Self {
        Self {
            manifest: RwLock::new(Arc::new(manifest)),
            kill,
            replay: Mutex::new(BoundedIdSet::new(10_000)),
        }
    }

    pub fn manifest(&self) -> Arc<ActionManifest> {
        self.manifest.read().expect("lock").clone()
    }

    /// Replaces the manifest after a re-discovery (server capabilities
    /// changed).
    pub fn replace_manifest(&self, manifest: ActionManifest) {
        *self.manifest.write().expect("lock") = Arc::new(manifest);
    }

    pub fn kill_switch(&self) -> &KillSwitch {
        &self.kill
    }

    /// Full dispatch flow. Returns either an outcome or a rejection — a
    /// rejection means the command NEVER reached MT5.
    ///
    /// # Errors
    /// Transport-level failures surface as `McpError`; authorization-level
    /// failures surface as `DispatchRejection`.
    pub async fn dispatch(
        &self,
        mcp: &mut McpHttpClient,
        raw: &str,
        ctx: &DispatchContext,
    ) -> Result<DispatchOutcome, DispatchRejection> {
        let started = Instant::now();
        let req: ActionRequest =
            serde_json::from_str(raw).map_err(|_| DispatchRejection::Malformed)?;

        let started_at = now_rfc3339();
        let finish = |status: ActionStatus,
                      payload: Option<Value>,
                      validation_errors: Vec<String>,
                      execution_error: Option<String>,
                      req: &ActionRequest| {
            DispatchOutcome {
                request_id: req.request_id.clone(),
                command_id: req.command_id.clone(),
                action_name: req.action_name.clone(),
                status,
                started_at: started_at.clone(),
                completed_at: now_rfc3339(),
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                actual_mcp_result: payload,
                validation_errors,
                execution_error,
                correlation_id: req.request_id.clone(),
            }
        };

        // 2. Kill switch (before anything else — spec: immediate block).
        let descriptor = self
            .manifest
            .read()
            .expect("lock")
            .get(&req.action_name)
            .cloned();
        let category = descriptor
            .as_ref()
            .map(|d| d.classification.category.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let is_financial = descriptor
            .as_ref()
            .is_some_and(|d| d.classification.tier == RiskTier::Financial);
        if let Some(scope) = self.kill.check(
            ctx.user_id.as_deref(),
            &req.account_id,
            &req.device_id,
            &category,
            is_financial,
        ) {
            tracing::warn!(action = %req.action_name, "dispatch rejected: kill switch");
            return Err(DispatchRejection::KillSwitchActive(scope));
        }

        // 3. Expiration.
        let expires = OffsetDateTime::parse(&req.expires_at, &Rfc3339)
            .ok()
            .or_else(|| crate::mt5::parse_broker_time(&req.expires_at))
            .ok_or(DispatchRejection::Malformed)?;
        if expires <= OffsetDateTime::now_utc() {
            tracing::warn!(action = %req.action_name, "dispatch rejected: expired");
            return Err(DispatchRejection::Expired);
        }

        // 4. Device binding.
        if req.device_id != ctx.device_id {
            tracing::warn!(action = %req.action_name, "dispatch rejected: wrong device");
            return Err(DispatchRejection::WrongDevice);
        }

        // 5. Account binding (cross-account isolation).
        if !ctx.authorized_accounts.contains(&req.account_id) {
            tracing::warn!(
                action = %req.action_name,
                account = %mask_login(&req.account_id),
                "dispatch rejected: account not authorized for this device"
            );
            return Err(DispatchRejection::WrongAccount);
        }

        // 6. Manifest resolution.
        let Some(descriptor) = descriptor else {
            tracing::warn!(action = %req.action_name, "dispatch rejected: unknown action");
            return Err(DispatchRejection::UnknownAction(req.action_name.clone()));
        };

        // 7. Payload size + schema validation.
        let params_bytes = serde_json::to_vec(&req.parameters)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if params_bytes > MAX_PARAM_BYTES {
            return Err(DispatchRejection::PayloadTooLarge {
                actual: params_bytes,
                limit: MAX_PARAM_BYTES,
            });
        }
        let validation_errors = validate_against_schema(&req.parameters, &descriptor.input_schema);
        if !validation_errors.is_empty() {
            tracing::warn!(action = %req.action_name, "dispatch rejected: schema");
            return Err(DispatchRejection::SchemaValidation(validation_errors));
        }

        // 8. Permission gate.
        let tier = descriptor.classification.tier;
        let required = descriptor.required_permission;
        let permission_ok = match tier {
            RiskTier::ReadOnly => true,
            RiskTier::LocalModification => ctx.grants.contains("local.modify"),
            RiskTier::FilesystemOrCode => ctx.grants.contains("advanced.execute"),
            RiskTier::Financial => ctx.grants.contains("trading.execute"),
        };
        if !permission_ok {
            tracing::warn!(action = %req.action_name, tier = %tier.as_str(), "dispatch rejected: permission");
            return Err(DispatchRejection::PermissionDenied {
                tier,
                required: required.to_string(),
            });
        }
        if tier == RiskTier::Financial && !ctx.financial_consent {
            tracing::warn!(action = %req.action_name, "dispatch rejected: no financial consent");
            return Err(DispatchRejection::FinancialConsentMissing);
        }

        // 9. Replay protection — only successful validations consume an id.
        {
            let mut replay = self.replay.lock().expect("lock");
            if !replay.insert(&req.idempotency_key) {
                tracing::warn!(action = %req.action_name, "dispatch rejected: replay");
                return Err(DispatchRejection::Replayed);
            }
        }

        // 10. Real execution.
        tracing::info!(
            action = %req.action_name,
            tier = %tier.as_str(),
            correlation_id = %req.request_id,
            "executing authorized MCP action"
        );
        let envelope = mcp.call_tool_raw(&req.action_name, &req.parameters).await;
        match envelope {
            Ok(env) => match extract_tool_result(&env) {
                // JSON payloads pass through verbatim (raw MT5 data).
                Ok(ToolOutcome::Json(payload)) => Ok(finish(
                    ActionStatus::Executed,
                    Some(payload),
                    Vec::new(),
                    None,
                    &req,
                )),
                // LIVE-VERIFIED: mutation tools (add_marketwatch_symbol,
                // ...) answer with plain text. Record the kind explicitly so
                // the Platform can distinguish response shapes.
                Ok(ToolOutcome::Text(text)) => Ok(finish(
                    ActionStatus::Executed,
                    Some(json!({ "kind": "text", "text": text })),
                    Vec::new(),
                    None,
                    &req,
                )),
                Err(McpError::Malformed(msg)) => Ok(finish(
                    ActionStatus::McpFailed,
                    None,
                    Vec::new(),
                    Some(msg),
                    &req,
                )),
                Err(McpError::ToolFailed(msg)) => Ok(finish(
                    ActionStatus::McpFailed,
                    None,
                    Vec::new(),
                    Some(msg),
                    &req,
                )),
                Err(e) => Ok(finish(
                    ActionStatus::McpFailed,
                    None,
                    Vec::new(),
                    Some(e.to_string()),
                    &req,
                )),
            },
            Err(e) => Ok(finish(
                ActionStatus::McpFailed,
                None,
                Vec::new(),
                Some(e.to_string()),
                &req,
            )),
        }
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_display_is_informative() {
        assert!(DispatchRejection::Expired.to_string().contains("expired"));
        assert!(
            DispatchRejection::UnknownAction("x".into())
                .to_string()
                .contains("manifest")
        );
        assert!(
            DispatchRejection::FinancialConsentMissing
                .to_string()
                .contains("consent")
        );
        assert!(
            DispatchRejection::PermissionDenied {
                tier: RiskTier::Financial,
                required: "trading.execute".to_string()
            }
            .to_string()
            .contains("trading.execute")
        );
    }

    // Full dispatcher flows (allow/deny per tier, expiry, replay, kill
    // switch, schema validation, real execution) are covered by the
    // integration tests in tests/actions_tests.rs, which need the mock MCP
    // server.
}
