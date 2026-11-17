# NSE Connector — Core, Bridge Daemon, Test Suite

The secure bridge between a customer's local MetaTrader terminal and the NSE
Platform (`bridge.nexusverse.space`): a Rust core library, the **`nse-connector`
daemon (Windows EXE)**, and a 90-command broker-agnostic bridge surface
(`docs/BRIDGE_COMMANDS.md`).

```
NSE Platform ──WSS──▶ nse-connector.exe ──HTTP──▶ MetaTrader terminal
  (commands, acks)        (this repo)              (MCP, local only)
```

## The bridge daemon

```bat
:: one-time: persist secrets into Windows Credential Manager (never files)
set NSE_API_KEY=nse_live_...
set MT5_MCP_TOKEN=...
target\release\nse-connector.exe store-secrets

:: run the bridge
target\release\nse-connector.exe
```

Configuration (environment, all optional unless noted): `NSE_PLATFORM_URL`
(default `https://bridge.nexusverse.space`), `NSE_API_KEY` (required; or
stored), `NSE_DEVICE_ID` (else generated + persisted under
`%APPDATA%\nse-connector\`), `MT5_MCP_URL` (default
`http://127.0.0.1:22346/mcp`), `MT5_MCP_TOKEN`, `NSE_SYNC_INTERVAL_SECS`
(default 60), `RUST_LOG`.

Boot: validate key → register device → MT5 handshake → action-manifest
discovery (107 live actions, tier-classified) → platform WebSocket.
Runtime: platform `command` frames run the full local gate chain (kill
switch → expiry → device/account binding → tier permission → financial
consent → replay) and answer `action.result`; periodic/`sync_request` ticks
push bounded account/position/history batches; Ctrl+C shuts down gracefully.

## The 90-command bridge surface

`docs/BRIDGE_COMMANDS.md` is the authoritative table (also live via
`get_bridge_capabilities`). Every command answers the same JSON envelope —
`{command, ok, data, broker_time, error}` — so the Platform never needs
broker knowledge. Categories: terminal, account, symbols, market, chart,
positions, orders, trading execution, risk calculations, diagnostics.
Fields the broker does not expose are answered `not_available` with the
reason; derivations (pip value, margin level, position sizing…) expose
every intermediate value.

## Layout

```
crates/connector-core/
├── src/
│   ├── bin/nse_connector.rs  The daemon (composition root, graceful shutdown)
│   ├── bridge/               90-command surface (registry + handlers)
│   ├── mcp/        MCP streamable-HTTP client (handshake, Bearer auth,
│   │               sessions, tool discovery, JSON-in-text extraction,
│   │               session-loss recovery)
│   ├── mt5/        Tolerant MT5 models (LIVE-verified field names) + mapping
│   ├── actions/    Action architecture: manifest discovery + count
│   │               validation, Tier 0-3 risk classification, schema
│   │               validation, universal dispatcher gate chain, kill
│   │               switch, NSE JSON envelopes, symbol registry
│   ├── nse/        NSE Core gateway client (PROPOSED contract) + WSS socket
│   ├── sync/       Bounded sync engine (dedup, checkpoints, acks)
│   └── security/   Secret store (Windows Credential Manager), redaction,
│                   Tier-3 command validator
└── tests/          Mock MCP/NSE integration + REAL MT5 integration (env-gated)
```

## Running the tests

### Deterministic suite (no MT5 needed)

```bash
cargo test --workspace
```

155 tests across all spec §16 categories PLUS the 15-scenario WebSocket reliability suite (`tests/ws_reliability_tests.rs`, real-TCP mock Platform); mocks inject every failure mode
(bad auth, malformed JSON, timeouts, session loss, expired/revoked keys,
kill switch, replays...).

### REAL MT5 integration tests (the customer-PC compatibility check)

```bat
set NSE_LIVE_MCP_TESTS=1
set NSE_MCP_TEST_TOKEN=<your MT5 MCP API key>
cargo test --test live_mt5_tests -- --ignored --nocapture
```

The token lives only in the environment for the run — never written to any
file. Covers: handshake, discovery, the trading tools, error behaviors
(401/404/-32602), the full manifest with risk classification, and a Tier-0
dispatch through the complete gate chain.

### Quality gates

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

## Live-verified wire facts (MetaTrader 5 MCP v1.0, build 6306)

- Bearer auth mandatory: missing/wrong token → HTTP 401.
- Session via `Mcp-Session-Id` response header; unknown session → HTTP 404
  (empty body); the client re-initializes once and recovers.
- `notifications/initialized` → HTTP 202.
- Unsupported protocol versions are negotiated down — verify the negotiated
  version.
- Unknown tool → HTTP 200 + JSON-RPC `-32602 "tool not found"`.
- Tool payloads arrive as JSON-in-text (`result.content[0].text`); mutation
  tools answer plain text with `isError:false`.
- Record field names: `position_id` / `order_id` / `deal_id`, `action`,
  `create_time` / `open_time` (ISO without timezone), `price_last`,
  `price_open`, `contract_size` — aliases accepted via serde.
- 107 actions discovered live; tiers: 41 read-only, 26 local, 34
  filesystem/code, 6 financial.
- Broker facts: no leverage field anywhere; `tick_value`/`tick_size` can be
  0.0 (derive via calc tools); `trade_close_single_position` has no volume
  parameter (partial close impossible via MCP); pending-order price/volume
  modification not exposed.
- Terminal gate: order execution additionally requires the terminal's own
  MCP trading permission — when disabled, trading tools answer
  `"trading is not permitted"` even with all flags true (read tools work).

## Honest status

| Area | Status |
|---|---|
| MCP client + MT5 data path | **Live-verified** against the real server |
| Action manifest + tiers | **Live-verified** (107 actions classified) |
| Dispatcher gate chain | Mock-verified (mocks reproduce live wire behavior) |
| 90-command bridge surface | Implemented; registry/calculators unit-tested; live handlers verified via live tests |
| Bridge daemon EXE | **Built** (`target\release\nse-connector.exe`) |
| NSE Core REST/WSS contract | PROPOSED — mock-verified only; reconcile against the deployed API (`docs/PROPOSED_NSE_PLATFORM_INTEGRATION.md`) |
| Order execution E2E | Blocked by the terminal's MCP trading permission (see wire facts); everything else verified |
| Windows GUI, installer | Not started (next phase) |
| Secret storage | Windows Credential Manager round-trip tested on this machine |

