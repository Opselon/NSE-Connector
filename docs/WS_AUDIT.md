# WebSocket Architecture Audit + Reliability Test Report

Directive: WebSocket-first development cycle (remote Platform ↔ Connector).
Scope lock honored: NO MCP permission changes, NO trading execution, NO new
MT5 control paths. Existing MCP functionality untouched.

## 1. Architecture audit

Both sides of the wire were audited against real code, not assumptions:

- **Server side (as-built):** `Opselon/NSE-Platform` @ PR #103 branch —
  `backend/src/modules/connector/ws.rs`, `commands.rs` (frame builder,
  replay, kill-switch gates, `WS_DEATH_SECS=45`, inbound rate limit
  120/min, pre-upgrade auth, NO hello frame, heartbeat replies carry
  `pending_commands`, `action.result` requires `correlation_id` +
  `protocol_version=="1.0"`, `manifest.update` accepted from client).
- **Client side:** `connector-core` `nse/ws.rs` (transport), `nse/gateway.rs`
  (auth+REST), `nse/session.rs` (frame wiring), `actions/dispatcher.rs` +
  `bridge/` (command execution), `sync/` (batches/checkpoints).

### Verdict per directive §4 requirement

| §4 requirement | Status BEFORE | Status AFTER this cycle |
|---|---|---|
| Outbound-only remote WS | ✅ compliant | ✅ unchanged (verified: nothing listens locally) |
| No duplicate device registration | ✅ | ✅ + stable persisted device id documented |
| No duplicate command execution after reconnect | ❌ RAM-only replay cache | ✅ persistent bounded `CommandJournal` (at-most-once) |
| No silent message loss | ❌ unacked batches never re-delivered | ✅ pending batches re-delivered at the head of every sync cycle |
| Explicit acknowledgment semantics | ⚠️ partial | ✅ batch ack retires; `action.result` acks commands; server acks logged |
| Bounded retry policies | ✅ backoff capped | ✅ + journal window (10k) + queue eviction |
| Idempotent synchronization | ✅ | ✅ + original batch_id on re-delivery |
| Structured error logging / no noisy success | ❌ every batch at INFO | ✅ routine deliveries DEBUG; failures WARN with ids |
| No blocking in WS event loop | ✅ | ✅ + 45s inbound watchdog + bounded sends |
| Separation transport/auth/sync/commands | ✅ | ✅ frame pipeline moved INTO the library (`nse/session.rs`) — the daemon is now a thin composition root |

### Dialect reconciliation (the critical audit finding)

The as-built server sends command payloads inside `operation` (envelope
name + merged parameters) with NO `request_id` and NO separate
`parameters`; the connector previously expected the PROPOSED contract
(`action_name` + `parameters`). As-built frames would have been
**unroutable**. Now: dual-dialect parsing (`nse/command_frame.rs`),
envelope→action resolution, as-built `action.result` shape
(`correlation_id`, `protocol_version:"1.0"`, `result`), and
`manifest.update` announcements on every session start.

## 2. Reliability test report (directive §3 — 15/15 pass)

`cargo test --test ws_reliability_tests` — real TCP, real axum upgrades,
real abrupt disconnects; compressed timings (150 ms heartbeats, 1.5 s
watchdog, ≤300 ms backoff). Full run ≈ 5 s, fully deterministic.

| # | Scenario | Result |
|---|---|---|
| 1 | Initial connection + heartbeats + manifest announcement | ✅ |
| 2 | Auth failure (401 pre-upgrade) → backoff → recovery | ✅ |
| 3 | Invalid device credentials (403 pre-upgrade, headers verified) | ✅ |
| 4 | Server unavailable → bounded backoff, healthy shutdown | ✅ |
| 5 | Network disconnection → automatic reconnect | ✅ |
| 6 | Server restart + pending replay → exactly-once via journal | ✅ |
| 7 | Client restart → fresh process refuses executed commands | ✅ |
| 8 | Reconnection + session recovery + sync acks retire batches | ✅ |
| 9 | Duplicate frames in-session → one execution + one refusal | ✅ |
| 10 | Missing ack → retained + re-delivered → retires on ack | ✅ |
| 11 | Message ordering (3 commands → in-order results) | ✅ |
| 12 | Accept-but-never-upgrade → bounded connect timeout | ✅ |
| 13 | Heartbeat failure → 45 s-equivalent watchdog fires | ✅ |
| 14 | Long-running churn (drops + replays) → sane counters, responsive | ✅ |
| 15 | Graceful shutdown under load → prompt, supervisor joins | ✅ |

Two REAL defects were found BY this suite and fixed:
1. Unacked sync batches were never re-delivered (§4 violation — no silent
   message loss). Fixed: pending batches re-deliver at the head of every
   sync cycle with their original ids.
2. Journal FIFO eviction was HashSet-order (nondeterministic). Fixed:
   explicit insertion-order tracking.

## 3. Connection lifecycle metrics

Emitted via `tracing` fields (daemon boot → runtime → shutdown) and
`BridgeState` (available to `get_bridge_status`/`get_bridge_health`/
`get_mt5_connection_metrics` bridge commands):

- `sessions` — successful WS sessions since spawn (`ManagedSocket::sessions`)
- session age, reconnects, last error (connection metrics command)
- commands executed / rejected / mcp_failed, avg + last latency
- pending batch count at shutdown
- batch delivery latency per batch (`duration_ms` in logs)

## 4. Real-server evidence

`bridge.nexusverse.space` currently has NO nginx vhost (requests fall
through to the default site) and the deployed container image is built
from `main` WITHOUT the connector module (PR #103 is unmerged) — verified
read-only on the deployment server. Evidence therefore runs against a
real-TCP contract-faithful mock Platform (standalone server semantics:
upgrade, replay, live pushes, abrupt drops). The moment PR #102/#103 merge
and the vhost points at the backend, the same suite runs against production
— `Harness::boot` only needs the real base URL.

## 5. Quality gates (this branch)

- `cargo fmt --check` ✅
- `cargo clippy --workspace --all-targets -- -D warnings` ✅
- `cargo test --workspace` ✅ **155 tests, 0 failures** (140 previous suite
  + 15 reliability + 8 command-frame/journal unit tests)
- Live MT5 tests: 10/10 (`NSE_LIVE_MCP_TESTS=1`) — unchanged wire behavior
- New: minimal GitHub Actions CI (fmt/clippy/test) so the PR loop is
  enforceable
