# NSE Connector — Error Classification Catalog (v1, directive §4)

Seeded from live E2E evidence (`docs/e2e-evidence/`). Every entry carries:
raw message → normalized code → cause → recovery → retry-safety.
This catalog grows as the remaining actions are swept (37 items still
`live-pending` in the capability matrix).

## 1. Normalized error JSON (connector → Platform)

```json
{
  "protocol_version": "1.0",
  "schema_version": "2026-11-17",
  "message_id": "uuidv7",
  "correlation_id": "request/command id",
  "event_type": "action.result",
  "status": "rejected | executed | mcp_failed",
  "error": {
    "code": "<normalized code>",
    "category": "<category>",
    "source_action": "<mcp action>",
    "raw": "<verbatim server text>",
    "cause": "<one line>",
    "recovery": "<strategy>",
    "retry_safe": true | false,
    "server_intervention_required": true | false
  }
}
```

## 2. Live-captured errors (verified against real MT5 MCP)

| Code | Category | Raw message (verbatim) | Source action(s) | Cause | Recovery | Retry safe | Evidence |
|---|---|---|---|---|---|---|---|
| `SYMBOL_NOT_IN_MARKET_WATCH` | Prerequisite | `symbol not found in market watch` | `trade_order_calc_margin`, `trade_order_calc_profit`, `trade_check_market_order` | Calc/trade tools require the symbol in Market Watch | `add_marketwatch_symbol` → retry → optionally remove | Yes (after prerequisite) | E011, E901(pre-fix) |
| `BOOK_SUBSCRIPTION_REQUIRED` | Prerequisite | `subscription not found, use market_book_subscribe first` | `market_book_get` | Order book requires an active subscription | `market_book_subscribe` → `market_book_get` → `market_book_unsubscribe` | Yes | E008, E102 |
| `BOOK_SUBSCRIPTION_NOT_ACTIVE` | Prerequisite | `subscription not found` variants on unsubscribe/get | `market_book_unsubscribe` (×2), `market_book_get` (after unsubscribe) | Subscription already released / never opened | Treat as state mismatch; re-sync subscription state | Yes (no side effects) | E113, E114 |
| `VOLUME_INVALID` | Invalid parameters | `volume must be greater than 0` | calc/trade tools | volume ≤ 0 or non-finite | Fix request; reject locally before sending | **No** (client bug, not transient) | E902 |
| `PARAMETER_MISSING` | Invalid parameters | `symbol must be specified` | `market_book_get` | Required parameter omitted | Local schema validation should catch first | No | E907 |
| `SYMBOL_INVALID` | Invalid parameters / broker | `symbol not found` (INVALID_SYM_XY probe) | calc/trade tools | Symbol does not exist on this broker | Do not retry; flag mapping failure for the symbol registry | **No** | E108 |
| `TRADING_NOT_PERMITTED` | Broker restriction / market state | `trading is not permitted` | `trade_check_market_order` | Market closed (Sunday, all trade flags true) **or** terminal/account trade-disabled | Distinguish via `terminal_get_info` flags + session schedule; retry at market open, **re-validate before execution** | Conditional — never retry financial actions blind | E105–E108 |
| `MARKET_WATCH_REMOVE_BLOCKED` | Precondition | `market watch symbol cannot be removed because it is currently used by an active chart, open position, pending order, or running expert/script` | `remove_marketwatch_symbol` | Symbol in use by terminal state | Skip cleanup; leave symbol in Market Watch | No (state-protected) | E199 |
| `TOOL_NOT_FOUND` | MCP protocol | JSON-RPC `-32602 "tool not found"` (HTTP 200) | any unknown action | Action absent from current manifest | Dispatcher rejects before execution (`UnknownAction`) | No | E908 |
| `TEXT_RESPONSE_SUCCESS` | Response-shape | plain text body with `isError:false` (e.g. `market watch symbol added`) | mutation tools (E100) | Server answers non-JSON for some mutations | Dispatcher records `{"kind":"text"}` result — not an error | n/a | E100 |

## 3. Transport/protocol classes (verified on mocks + live)

| Code | Category | Trigger | Recovery | Retry safe |
|---|---|---|---|---|
| `MCP_UNAUTHORIZED` | Auth | Missing/wrong bearer → HTTP 401 + `WWW-Authenticate: Bearer` | Re-prompt credential; **no auto-retry** | No |
| `MCP_SESSION_EXPIRED` | Protocol | Unknown session → HTTP 404 (empty body) | One transparent re-initialize (client), then report | Yes |
| `MCP_TIMEOUT` | Timeout | Server exceeds request timeout (mock hang test) | Cancel; retry read actions; **never blind-retry Tier 3** | Read: yes · financial: no |
| `MCP_MALFORMED` | Protocol | Garbage body / non-JSON where JSON required | Report with raw excerpt; no retry | No |
| `MCP_PROTOCOL_MISMATCH` | Protocol | Server negotiates an unsupported protocolVersion | Refuse + report negotiated version | No |
| `TOOL_IS_ERROR` | Tool failure | `result.isError: true` with text | Classify by text → table above | Per cause |
| `NSE_KEY_INVALID` / `NSE_KEY_REVOKED` | Auth (Platform, PROPOSED) | 401 `key_invalid`/`key_revoked` | Stop sync; user action required | No |
| `NSE_SUBSCRIPTION_EXPIRED` / `NSE_DEVICE_LIMIT` / `NSE_ACCOUNT_NOT_AUTHORIZED` / `NSE_FEATURE_NOT_ENABLED` | Authorization (Platform, PROPOSED) | 403 codes | Disable gated features; keep status visible | No |
| `NSE_SERVER_UNAVAILABLE` | Platform availability | 5xx | Exponential backoff + jitter; bounded queue keeps batches | Yes (reads/batches) |

## 4. Rules

1. **Financial actions are never blind-retried.** A timeout or transport
   failure during a Tier-3 command triggers: local state check
   (positions/orders read-back) → reconciliation report → manual/server
   decision. Uniqueness via `idempotency_key` prevents double execution.
2. Prerequisite errors (`SYMBOL_NOT_IN_MARKET_WATCH`,
   `BOOK_SUBSCRIPTION_REQUIRED`) are **connector-orchestrated**: the
   Connector performs the setup call, retries once, and only surfaces the
   error if it persists.
3. Every result (success or failure) carries the raw server text in `error.raw`
   — the Platform always receives the verbatim broker/MCP message alongside
   the normalized code (spec §5/§7 broker-compatibility law).
