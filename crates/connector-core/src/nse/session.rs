//! Platform session pipeline — processes inbound WebSocket frames exactly
//! as the daemon does, as library code so reliability tests exercise the
//! SAME path production runs (no duplicated wiring in tests).
//!
//! Separation (§4): transport lives in [`super::ws`], authentication in
//! [`super::gateway`], synchronization in [`crate::sync`] + this module's
//! sync cycle, command handling in the dispatcher + bridge registry. This
//! module is only the WIRING between them.

use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

use crate::actions::dispatcher::{ActionDispatcher, DispatchContext};
use crate::bridge;
use crate::mcp::McpHttpClient;
use crate::nse::command_frame::IncomingCommand;
use crate::nse::command_journal::CommandJournal;
use crate::nse::contract::SyncBatchRequest;
use crate::nse::ws::{ServerMessage, parse_server_message};
use crate::sync::SyncEngine;

/// Everything one inbound frame may touch, bundled for small signatures.
pub struct SessionDeps<'a> {
    pub mcp: &'a mut McpHttpClient,
    pub ctx: &'a mut DispatchContext,
    pub dispatcher: &'a ActionDispatcher,
    pub engine: &'a mut SyncEngine,
    pub gateway: &'a crate::nse::gateway::NseGatewayClient,
    pub state: &'a Arc<bridge::BridgeState>,
    pub journal: &'a Arc<CommandJournal>,
    /// Outbound socket half — results/acks ride back through the SAME
    /// connection they arrived on.
    pub outbound: &'a tokio::sync::mpsc::Sender<String>,
}

/// Processes one inbound platform frame (command / sync_request / ack /
/// heartbeat). Unknown frames are logged and ignored — a malformed frame
/// must never crash the session.
pub async fn handle_platform_frame(text: &str, deps: &mut SessionDeps<'_>) {
    match parse_server_message(text) {
        Some(ServerMessage::Command(frame_value)) => {
            handle_command(&frame_value, deps).await;
        }
        Some(ServerMessage::SyncRequest) => {
            tracing::info!("platform requested a sync");
            sync_cycle(deps).await;
        }
        Some(ServerMessage::Ack { batch_id }) => {
            if deps.engine.mark_acked(&batch_id) {
                tracing::debug!(batch_id = %batch_id, "batch acknowledged");
            }
        }
        Some(ServerMessage::AckGeneric) => tracing::debug!("platform ack received"),
        Some(ServerMessage::HeartbeatOk) => tracing::trace!("platform heartbeat ok"),
        None => tracing::warn!(frame = %text, "unparseable server frame ignored"),
    }
}

/// Command execution path, shared by both wire dialects: journal dedup (§4)
/// → gate chain → bridge/native execution → as-built `action.result` back
/// through the SAME socket.
async fn handle_command(frame_value: &Value, deps: &mut SessionDeps<'_>) {
    let cmd = match IncomingCommand::parse(frame_value) {
        Ok(c) => c,
        Err(reason) => {
            tracing::warn!(reason = %reason, "unroutable command frame — no result sent");
            return;
        }
    };
    // §4 no-duplicate-execution: journal FIRST, dispatch SECOND. A replayed
    // command gets its duplicate refusal reported instead of a re-execution.
    if deps.journal.contains(&cmd.command_id) {
        tracing::warn!(
            command_id = %cmd.command_id,
            action = %cmd.action_name,
            "replayed command already executed — refusing duplicate execution"
        );
        deps.state
            .observe_command(&cmd.action_name, bridge::CommandOutcome::Rejected, 0);
        send_result(
            deps.outbound,
            &cmd,
            "rejected",
            None,
            Some("command already executed (journal)".into()),
            0,
        )
        .await;
        return;
    }
    if let Err(e) = deps.journal.record(&cmd.command_id, "dispatched") {
        tracing::error!(
            command_id = %cmd.command_id,
            error = %e,
            "journal unavailable — refusing unrecorded execution"
        );
        send_result(
            deps.outbound,
            &cmd,
            "mcp_failed",
            None,
            Some("journal unavailable — command not executed".into()),
            0,
        )
        .await;
        return;
    }
    let req = cmd.to_action_request();
    let started = Instant::now();
    if let Some(spec) = bridge::lookup(&req.action_name) {
        // ---- Bridge command path (90-command surface) ----
        let replay_ok = |key: &str| deps.dispatcher.check_replay(key);
        match bridge::gate_check(
            &req,
            deps.ctx,
            deps.dispatcher.kill_switch(),
            replay_ok,
            spec,
        ) {
            Ok(()) => {
                let manifest_arc = deps.dispatcher.manifest();
                let mut bdeps = bridge::BridgeDeps {
                    mcp: deps.mcp,
                    manifest: &manifest_arc,
                    state: deps.state,
                };
                let result = bridge::execute(&req.action_name, &req.parameters, &mut bdeps).await;
                let latency = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let outcome = match &result {
                    Ok(_) => bridge::CommandOutcome::Executed,
                    Err(f) if f.code == "mt5_error" => bridge::CommandOutcome::McpFailed,
                    Err(f) if f.code == "invalid_params" => bridge::CommandOutcome::Rejected,
                    // not_available is a truthful capability answer, not a failure.
                    Err(_) => bridge::CommandOutcome::Executed,
                };
                deps.state
                    .observe_command(&req.action_name, outcome, latency);
                if matches!(outcome, bridge::CommandOutcome::McpFailed)
                    && let Err(f) = &result
                {
                    deps.state
                        .set_last_error(format!("{}: {}", req.action_name, f.message));
                }
                let (status, error) = match &result {
                    Ok(_) => ("executed", None),
                    Err(f) if f.code == "mt5_error" => ("mcp_failed", Some(f.message.clone())),
                    Err(f) if f.code == "invalid_params" => ("rejected", Some(f.message.clone())),
                    // not_available is the envelope's truthful verdict.
                    Err(_) => ("executed", None),
                };
                let envelope = bridge::envelope(&req.action_name, result);
                send_result(deps.outbound, &cmd, status, Some(envelope), error, latency).await;
            }
            Err(rejection) => {
                deps.state
                    .observe_command(&req.action_name, bridge::CommandOutcome::Rejected, 0);
                send_result(
                    deps.outbound,
                    &cmd,
                    "rejected",
                    None,
                    Some(rejection.to_string()),
                    0,
                )
                .await;
            }
        }
    } else {
        // ---- Native MCP action path (manifest action names) ----
        let raw = serde_json::to_string(&req).unwrap_or_default();
        match deps.dispatcher.dispatch(deps.mcp, &raw, deps.ctx).await {
            Ok(outcome) => {
                deps.state.observe_command(
                    &outcome.action_name,
                    bridge::CommandOutcome::Executed,
                    outcome.duration_ms,
                );
                send_result(
                    deps.outbound,
                    &cmd,
                    outcome.status.as_str(),
                    outcome.actual_mcp_result.clone(),
                    outcome.execution_error.clone(),
                    outcome.duration_ms,
                )
                .await;
            }
            Err(rejection) => {
                deps.state
                    .observe_command(&req.action_name, bridge::CommandOutcome::Rejected, 0);
                send_result(
                    deps.outbound,
                    &cmd,
                    "rejected",
                    None,
                    Some(rejection.to_string()),
                    0,
                )
                .await;
            }
        }
    }
}

/// Unified `action.result` emitter — the as-built Platform shape:
/// `correlation_id` (== command_id) is required, `protocol_version` must be
/// `"1.0"` when present, and the payload rides in `result`. Proposed-dialect
/// compatibility fields ride along; the server ignores unknown keys.
async fn send_result(
    outbound: &tokio::sync::mpsc::Sender<String>,
    cmd: &IncomingCommand,
    status: &str,
    payload: Option<Value>,
    execution_error: Option<String>,
    duration_ms: u64,
) {
    let rejected = status == "rejected";
    let frame = json!({
        "type": "action.result",
        "correlation_id": cmd.command_id,
        "command_id": cmd.command_id,
        "protocol_version": crate::nse::WS_PROTOCOL_VERSION,
        "status": status,
        "result": payload.clone().unwrap_or(Value::Null),
        "execution_error": execution_error.clone(),
        "validation_errors": if rejected && execution_error.is_some() {
            json!([execution_error.clone().unwrap_or_default()])
        } else {
            Value::Null
        },
        "duration_ms": duration_ms,
        // proposed-contract compat fields
        "request_id": cmd.request_id,
        "actual_mcp_result": payload,
        "account_id": cmd.account_id,
        "device_id": cmd.device_id,
        "action_name": cmd.action_name,
    });
    tracing::info!(
        command_id = %cmd.command_id,
        action = %cmd.action_name,
        status,
        duration_ms,
        "action result reported"
    );
    if outbound.send(frame.to_string()).await.is_err() {
        tracing::warn!("failed to queue action result — socket backpressured");
    }
}

/// Pulls account/positions/history from the terminal and delivers bounded
/// batches to the platform. Failures are logged and retained for retry — a
/// failed cycle never drops data.
pub async fn sync_cycle(deps: &mut SessionDeps<'_>) {
    deps.state.note_sync();
    // §4 no-silent-message-loss: unacknowledged batches are re-delivered
    // first, in their original order with their original ids (idempotent
    // server-side via batch_id dedup). Retry bound = queue eviction.
    let pending: Vec<crate::sync::SyncBatch> = deps.engine.pending().cloned().collect();
    for batch in pending {
        deliver(deps, batch, false).await;
    }
    // Account snapshot (also extends local account binding on first sight).
    match deps
        .mcp
        .call_tool_json(crate::mcp::trading_tools::ACCOUNT_INFO, &json!({}))
        .await
    {
        Ok(payload) => match crate::mt5::account_snapshot(&payload) {
            Ok(snap) => {
                if let Some(login) = snap.account.login.clone()
                    && !login.is_empty()
                {
                    // Builder takes self by value; swap the updated copy in.
                    let updated = deps.ctx.clone().with_account(login);
                    *deps.ctx = updated;
                }
                if let Some(batch) = deps.engine.build_account_batch(&snap) {
                    deliver(deps, batch, true).await;
                }
            }
            Err(e) => {
                deps.state.set_last_error(format!("account mapping: {e}"));
                tracing::warn!(error = %e, "account snapshot mapping failed");
            }
        },
        Err(e) => {
            deps.state.set_last_error(format!("account query: {e}"));
            tracing::warn!(error = %e, "account query failed");
        }
    }

    // Open positions + working orders.
    match deps
        .mcp
        .call_tool_json(
            crate::mcp::trading_tools::OPEN_POSITIONS,
            &json!({ "include_orders": true }),
        )
        .await
    {
        Ok(payload) => match crate::mt5::open_positions(&payload) {
            Ok(pos) => {
                if let Some(batch) = deps.engine.build_positions_batch(&pos) {
                    deliver(deps, batch, true).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "positions mapping failed"),
        },
        Err(e) => tracing::warn!(error = %e, "positions query failed"),
    }

    // History window from the checkpoint (gap recovery after downtime).
    let to = crate::bridge::broker_time_now();
    let (from, _) = deps.engine.history_query_range(&to);
    let args = json!({
        "datetime_from": from,
        "datetime_to": to,
        "limit": deps.engine.history_page_limit(),
    });
    match deps
        .mcp
        .call_tool_json(crate::mcp::trading_tools::HISTORY_ORDERS, &args)
        .await
    {
        Ok(payload) => match crate::mt5::history_orders(&payload) {
            Ok(hist) => {
                let batch = deps.engine.build_history_batch(&hist);
                deliver(deps, batch, true).await;
            }
            Err(e) => tracing::warn!(error = %e, "history mapping failed"),
        },
        Err(e) => tracing::warn!(error = %e, "history query failed"),
    }
}

/// REST delivery → retire on accepted ack; retained on failure. `enqueue`
/// is false for pending-batch re-delivery (already queued — re-queuing
/// would duplicate it in the bounded queue).
async fn deliver(deps: &mut SessionDeps<'_>, batch: crate::sync::SyncBatch, enqueue: bool) {
    if enqueue {
        deps.engine.queue(batch.clone());
    }
    let req = SyncBatchRequest {
        batch_id: batch.batch_id.clone(),
        kind: batch.kind.as_str().to_string(),
        items: batch.items.clone(),
        count: batch.count as u32,
        produced_at: batch.produced_at.clone(),
    };
    match deps.gateway.send_batch(&req).await {
        Ok(ack) if ack.accepted => {
            deps.engine.mark_acked(&batch.batch_id);
            // §4 no-noisy-success-logs: routine deliveries are debug-level.
            tracing::debug!(
                batch_id = %batch.batch_id,
                kind = %batch.kind.as_str(),
                count = batch.count,
                "batch delivered and acknowledged"
            );
        }
        Ok(ack) => {
            tracing::warn!(
                batch_id = %batch.batch_id,
                accepted = ack.accepted,
                "batch not accepted; retained for retry"
            );
        }
        Err(e) => {
            tracing::warn!(
                batch_id = %batch.batch_id,
                error = %e,
                "batch delivery failed; retained for retry"
            );
        }
    }
}

/// Builds the as-built `manifest.update` frame announcing this connector's
/// manifest after discovery/re-discovery.
pub fn manifest_update_frame(manifest: &crate::actions::manifest::ActionManifest) -> Value {
    json!({
        "type": "manifest.update",
        "manifest_hash": format!("{:x}", manifest.content_hash),
        "action_count": manifest.action_count,
        "connector_version": env!("CARGO_PKG_VERSION"),
        "protocol_version": crate::nse::WS_PROTOCOL_VERSION,
    })
}
