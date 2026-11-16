# PROPOSED — NSE Platform ↔ NSE Connector Integration Contract

> **STATUS: PROPOSED, NOT VERIFIED.** The deployed NSE Core API
> (`bridge.nexusverse.space`) could NOT be inspected in this session — the
> `github.com/Opselon/NSE-Platform` repository was unreachable from this
> environment (TLS timeout) and no live Platform API was queried. Everything
> below is the minimum contract the Connector implementation requires,
> documented per engineering spec §10 ("If the required endpoint or
> capability does not exist: document the exact missing contract... Do not
> create a new backend"). Before release, every path/code must be reconciled
> against the real Platform. **No test result in this repo may be reported
> as verified NSE integration** — NSE-side tests run against a mock only.

## 0. Principles

- NSE Platform stays the sole authority for identity, subscription,
  entitlements, roles, risk, and persistence. The Connector adds no tables,
  no accounts, no subscriptions.
- The MT5 MCP credential never leaves the customer's machine (verified by
  test: `mt5_credential_never_reaches_nse_core`).
- Every command is re-authorized locally; delivery is not execution.

## 1. Authentication / Device APIs

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/connector/validate-key` | POST | API key + device validation → entitlements |
| `/api/v1/connector/register-device` | POST | Device pairing / re-registration |
| `/api/v1/connector/session/refresh` | POST | Session renewal |
| `/api/v1/connector/session/revoke` | POST | Logout / revocation |

**Headers (all endpoints):** `Authorization: Bearer <NSE API key>` (the key
travels ONLY here), `X-NSE-Device-Id: <device id>`.

**`validate-key` request body** (no secrets):
```json
{ "device_id": "…", "connector_version": "0.1.0" }
```

**`validate-key` response 200** — the authoritative entitlement set:
```json
{
  "valid": true,
  "user_id": "server-resolved-uuid",
  "subscription": { "active": true, "plan": "…", "expires_at": "RFC3339" },
  "entitlements": {
    "history_sync": true, "ai_coach": true, "position_manager": true,
    "auto_trade": false, "max_devices": 3, "max_accounts": 2
  }
}
```
Secure default: absent fields deserialize to *disabled*.

**Error contract** (`{"error": {"code": "…", "message": "…"}}`):
401 → `key_invalid` | `key_revoked`; 403 → `subscription_expired` |
`device_limit_exceeded` | `account_not_authorized` | `feature_not_enabled`;
429 → `rate_limited`. The Connector maps each code to a distinct user-facing
state and **stops unauthorized synchronization immediately**.

## 2. Synchronization APIs

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/connector/sync/batch` | POST | One bounded batch (account / positions / history_orders / history_positions) |
| `/api/v1/connector/heartbeat` | POST | Device keep-alive |
| `/api/v1/connector/ws` | GET (upgrade) | Persistent WSS command/ack channel |

**Batch body** (`schema_version: "2026-11-17"`, spec §6 envelope):
```json
{
  "protocol_version": "1.0", "schema_version": "2026-11-17",
  "message_id": "uuidv7", "correlation_id": "batch-uuid",
  "event_type": "sync.batch", "event_timestamp": "RFC3339",
  "device_id": "…", "user_id": "server-resolved", "account_id": "12345678901",
  "broker_id": "MetaQuotes Ltd.", "source_tool": "get_trading_history_orders",
  "payload": { "orders": [ … raw MT5 JSON … ], "deals": [ … ] },
  "data_quality": { "validation_warnings": [], "raw_available": true },
  "sync_metadata": { "checkpoint": "2026-11-16T23:04:07", "batch_kind": "history_orders" }
}
```
**Ack 200:** `{ "accepted": true, "batch_id": "…" }`. Unacked batches stay
in a bounded local queue (oldest evicted with a warning — never silently
discarded); recovery after downtime uses the server checkpoint + history
range queries.

**Broker time rule:** timestamps inside payloads are the broker's verbatim
strings (ISO without timezone on the live server, e.g.
`"2026-11-16T22:33:39"`). The Platform must store them as opaque broker
time plus the connector-reported UTC receipt time.

## 3. Remote Command API (WSS frames)

Server → Connector `command` frame (spec §6.2):
```json
{
  "type": "command",
  "request_id": "…", "command_id": "…", "action_name": "trade_modify_sl_tp",
  "account_id": "12345678901", "device_id": "…",
  "issued_at": "RFC3339", "expires_at": "RFC3339",
  "idempotency_key": "…",
  "parameters": { "ticket": "152913954047", "sl": 1.083, "tp": 1.087 },
  "authorization_context": { "user_id": "…", "grants": ["trading.execute"], "risk": {} }
}
```

Connector → Server result (spec §6.3, `event_type: "action.result"`):
`status` ∈ `rejected` | `executed` | `mcp_failed`, with `started_at`,
`completed_at`, `duration_ms`, `actual_mcp_result` (raw MT5 payload),
`validation_errors`, `execution_error`, `correlation_id`.

**Local gate chain (independent of the server, enforced by the Connector):**
kill switch → expiry → device binding → account binding → manifest
membership → input-schema validation → tier permission
(`local.modify` / `advanced.execute` / `trading.execute`) → explicit
financial consent for Tier 3 → replay protection → real MCP execution.
Rejections carry the reason back to the Platform.

**Tier 3 hard rules:** `auto_trade` entitlement + explicit customer consent
are BOTH required; an engaged kill switch (global/user/account/device/
auto_trade/category) blocks instantly; the six Tier-3 actions are exactly
`trade_send_market_order`, `trade_send_pending_order`,
`trade_close_single_position`, `trade_close_by_position`, `trade_modify_sl_tp`,
`trade_delete_order` (live-verified classification).

## 4. Manifest / Diagnostics APIs (needed from the Platform)

- Manifest registration: the Connector POSTs the discovered 107-action
  manifest (names, schemas, tiers, hash `b3df4d9fb2a3b7e0` for the current
  server) so the website can show per-action availability and broker
  compatibility.
- Diagnostics: connector health, MCP health, last sync, per-tier counts.

## 5. Open items for the Platform team

1. Confirm/adjust the endpoint paths + error codes above against the real
   deployment (this is a proposal, not a spec the Connector trusts blindly).
2. Provide the role/permission model so `authorization_context.grants` maps
   to real Platform roles (Viewer/Analyst/Trader/Bot Trader/...).
3. Confirm WSS frame schema + ack semantics (heartbeat interval, reconnect
   window, session expiry).
4. Decide where the action manifest is stored (existing Platform DB — the
   Connector must not introduce a new database).
