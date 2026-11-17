# NSE Connector ↔ NSE Platform — WebSocket Protocol (as-built)

Status: **reconciled with the deployed server code** (PR #103 branch,
`backend/src/modules/connector/ws.rs` + `commands.rs`). Where the
PROPOSED contract (`PROPOSED_NSE_PLATFORM_INTEGRATION.md`) differs, the
as-built dialect documented here wins; the connector accepts BOTH.

## Transport & authentication

| Property | Value |
|---|---|
| Endpoint | `GET wss://<platform>/api/v1/connector/ws` |
| Direction | **outbound-only** from the connector; the Platform never dials the customer machine; no inbound ports |
| Auth | `Authorization: Bearer <nse_live_…>` + `X-NSE-Device-Id` **in the upgrade headers** — never in frames or query strings |
| Rejection | invalid key/device → HTTP 401/403 **before** the upgrade (tested: `s02`, `s03`) |
| Hello frame | **none** — the connection is live immediately after the upgrade; the server immediately replays `pending` commands (trade-scoped keys) |

## Frame catalog

**Connector → Platform**

```json
{"type":"heartbeat"}                                     // every 15 s
{"type":"action.result",
 "correlation_id":"<command_id>",                        // REQUIRED by server
 "protocol_version":"1.0",                               // validated if present; must be "1.0"
 "status":"executed"|"rejected"|"mcp_failed",
 "result":{...}|null,
 "execution_error":"..."|null,
 "validation_errors":["..."]|null,
 "duration_ms":123,
 "command_id":"<command_id>",                            // compat
 "request_id":"..."|null,                                // proposed dialect compat
 "actual_mcp_result":{...}|null}                         // proposed dialect compat
{"type":"manifest.update","manifest_hash":"<hex>",
 "action_count":107,"connector_version":"0.1.0",
 "protocol_version":"1.0"}                               // sent after discovery/re-discovery
{"type":"ack", ...}                                      // for data.result/manifest.update acks (v1.1 data plane)
```

**Platform → Connector**

```json
{"type":"command","command_id":"<uuid v7>","account_id":"<login>",
 "device_id":"<device>","operation":{"type":"<envelope>", ...parameters merged...},
 "issued_at":"RFC3339","expires_at":"RFC3339","idempotency_key":"<command_id>"}
{"type":"heartbeat","server_time":"...","pending_commands":N}      // reply to client heartbeat
{"type":"sync_request","reason":"login_unknown"|"account_switched"}
{"type":"ack","correlation_id":..,"ok":bool}                       // result/manifest confirmations
```

**Dialect reconciliation** (`nse/command_frame.rs`): the as-built frame
carries the payload inside `operation` (object; `type` = envelope name,
remaining keys = parameters) and has no `request_id`. The connector accepts
both dialects, resolves envelopes (`modify_sl_tp` → `trade_modify_sl_tp`;
`close_position` → close-by/single/all by ticket shape) and synthesizes
`request_id := command_id`. Unknown envelope names pass through so the
manifest gate rejects them with a precise reason.

## Lifecycle

| Parameter | Connector | Platform (as-built) |
|---|---|---|
| Heartbeat interval | 15 s | death watchdog 45 s (3 missed beats) |
| Inbound liveness watchdog | **45 s** (`NseSocketConfig::death_timeout`) — any inbound frame restarts the window; silent peer ⇒ forced reconnect | — |
| Reconnect | exponential backoff 0.5 s → 30 s + jitter, abortable by shutdown | — |
| Connect timeout | 10 s (default) | — |
| Send timeout | 10 s — a timed-out send is a disconnect | — |
| Channels | bounded 128 in / 128 out | inbound rate limit 120/min |
| Replay on connect | handled via **persistent command journal** (see below) | replays `pending` commands (trade-scoped keys) |

## Delivery guarantees (§4)

| Requirement | Mechanism | Evidence |
|---|---|---|
| No duplicate device registration | stable UUIDv7 device id persisted under `%APPDATA%\nse-connector\`; register is an idempotent upsert | live boot log |
| No duplicate command execution after reconnect | `CommandJournal` — bounded (10k) JSONL at `%APPDATA%\nse-connector\executed_commands.jsonl`; id recorded **before** dispatch (at-most-once execution; the deliberate trade-off: a crash mid-execution loses that one command instead of duplicating it); replayed ids answered `rejected: already executed (journal)` | `s06`, `s07`, `s14` |
| No silent message loss | sync batches retained until acked and **re-delivered at the head of every sync cycle** (original ids — idempotent server-side via `batch_id` dedup); bounded queue eviction is the retry bound | `s10` |
| Explicit acknowledgment semantics | sync batches: REST `sync/batch` ack `{accepted,batch_id}` retires the batch; commands: `action.result` is the execution ack; server acks results/manifest.update | `s08`, `s10` |
| Bounded retry policies | pending queue FIFO eviction with WARN; backoff capped at 30 s; every send/connect/watchdog bounded | `s04`, `s12`, `s14` |
| Idempotent synchronization | item-level `(account, event_id)` dedup + checkpoint monotonicity (sync engine, 17 dedicated tests) | sync_tests |
| Structured error logging, no noisy success logs | `tracing` fields only; routine batch deliveries at DEBUG; failures at WARN with batch_id/error; command results at INFO (one line per command) | code |
| No blocking in the WS event loop | no std-lock across `.await` (clippy-enforced); watchdog/send/connect all timeout-bounded; channels bounded | audit |
| Separation transport / auth / sync / commands | `ws.rs` (transport) · `gateway.rs` (auth+REST) · `session.rs` (wiring+sync) · `dispatcher.rs`+`bridge/` (commands) | layout |

## Identified failure modes (tested)

| # | Failure mode | Connector behavior | Test |
|---|---|---|---|
| 1 | Bad API key | pre-upgrade 401; backoff retry; no crash; connects when auth clears | s02 |
| 2 | Invalid device binding | pre-upgrade 403; auth stays in headers | s03 |
| 3 | Server unreachable | connect refused; bounded backoff; clean shutdown | s04 |
| 4 | TCP drop mid-session | reconnect; sessions counter grows; sync recovery | s05, s08 |
| 5 | Server restart + command replay | journal: exactly-once execution; duplicates refused | s06, s07 |
| 6 | Duplicate command frames | first executes, duplicate answered `rejected` | s06, s09 |
| 7 | Missing batch ack | retained + re-delivered next cycle; retires on ack | s10 |
| 8 | Frame reordering risk | sequential processing on one socket; per-command results in order | s11 |
| 9 | Accept-but-never-upgrade server | bounded connect timeout; retry; no hang | s12 |
| 10 | Silent server (no heartbeat replies) | 45 s watchdog fires; forced reconnect | s13 |
| 11 | Long-run churn (drops + replays) | counters sane; exactly-once; responsive at end | s14 |
| 12 | Shutdown under load | prompt, supervisor joins | s15 |

## Known deviations / v1.1 items

- The as-built server treats ANY inbound text frame as liveness (client
  matches this; stricter heartbeat-only liveness is a server-side decision).
- Second socket for the same device overwrites the registry entry
  server-side (connector keeps a single socket, so not triggered).
- Retention/pruning for `chart_candles` and the executed-command journal
  file growth (10k window) — a server-side pruning job is the missing piece
  (recorded in PR #103 review).
- Real-server (deployed) evidence is pending merge of PR #102/#103 and the
  `bridge.nexusverse.space` nginx vhost; `tests/ws_reliability_tests.rs`
  runs against a real-TCP contract-faithful mock in the meantime.
