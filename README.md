# NSE Connector — Core + Test Suite

The core library and the full test matrix for the NSE Connector: the secure
bridge between a customer's local MetaTrader 5 MCP server and NSE Core
(`bridge.nexusverse.space`).

This repository contains the **foundation the Windows EXE is built on** —
not the EXE itself yet. Everything here is production Rust with a complete
test suite (engineering spec §16).

## Layout

```
crates/connector-core/
├── src/
│   ├── mcp/        MCP streamable-HTTP client (handshake, Bearer auth,
│   │               sessions, tool discovery, JSON-in-text extraction,
│   │               session-loss recovery)
│   ├── mt5/        Tolerant MT5 models (LIVE-verified field names) + mapping
│   ├── actions/    106-action architecture:
│   │   ├── manifest.rs     dynamic discovery + count validation (§3.1/3.2)
│   │   ├── risk.rs         Tier 0-3 classification (§7.3)
│   │   ├── schema.rs       input-schema validation (§3.3 step 3)
│   │   ├── dispatcher.rs   universal dispatcher gate chain (§3.3/§7)
│   │   ├── kill.rs         kill switch, all scopes (§7.5)
│   │   ├── contract.rs     NSE JSON envelopes (§6)
│   │   └── symbols.rs      broker symbol registry (§5)
│   ├── nse/        NSE Core gateway client (PROPOSED contract) + WSS socket
│   ├── sync/       Bounded sync engine (dedup, checkpoints, acks)
│   └── security/   Secret store (Windows Credential Manager), redaction,
│                   Tier-3 command validator
├── tests/
│   ├── common/     Mock MCP server (failure injection) + mock NSE Core
│   ├── mcp_tests.rs         spec §16 MCP cases (20)
│   ├── nse_tests.rs         spec §16 NSE cases (17)
│   ├── sync_tests.rs        spec §16 sync cases (17)
│   ├── security_tests.rs    spec §16 security cases (14)
│   ├── actions_tests.rs     106-action tests (17)
│   └── live_mt5_tests.rs    REAL MT5 integration (10, env-gated)
└── probe/          (workspace root) live-reconnaissance scratch — remove
                     before publishing the repo
```

## Running the tests

### Deterministic suite (no MT5 needed)

```bash
cargo test --workspace
```

122 tests: unit + mock integration across all spec §16 categories. Mock
servers inject every failure mode (bad auth, malformed JSON, timeouts,
session loss, expired/revoked keys, kill switch, replays...).

### REAL MT5 integration tests (the customer-PC compatibility check)

These run against the actual MetaTrader 5 MCP server on this PC — they
prove the Connector core works with a customer's real installation:

```bat
set NSE_LIVE_MCP_TESTS=1
set NSE_MCP_TEST_TOKEN=<your MT5 MCP API key>
set NSE_LIVE_MCP_ENDPOINT=http://127.0.0.1:22346/mcp   (optional)
cargo test --test live_mt5_tests -- --ignored --nocapture
```

The token lives only in the environment for the run — never written to any
file. Covers: handshake, tool discovery, the four trading tools, error
behaviors (401/404/-32602), the full 106-action manifest discovery with
risk classification, and a Tier-0 dispatch through the complete gate chain.

### Quality gates

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

## Live-verified wire facts (MetaTrader 5 MCP v1.0, build 6306)

- Bearer auth mandatory: missing/wrong token → HTTP 401 +
  `WWW-Authenticate: Bearer`.
- Session via `Mcp-Session-Id` response header; unknown session → HTTP 404
  (empty body); the client re-initializes once and recovers.
- `notifications/initialized` → HTTP 202.
- Unsupported protocol versions are negotiated down — the client must
  verify the negotiated version.
- Unknown tool → HTTP 200 + JSON-RPC `-32602 "tool not found"`.
- Tool payloads arrive as a JSON string inside `result.content[0].text`
  (JSON-in-text).
- Record field names: `position_id` / `order_id` / `deal_id`, `action`,
  `create_time` / `open_time` (ISO without timezone), `price_last`,
  `price_open`, `contract_size` — alternative naming variants are accepted
  through serde aliases.
- 107 actions discovered live (expected 106; the +1 is handled per spec
  §3.2 "more than expected").
- Tier classification over all live actions: 41 read-only, 26 local
  modifications, 34 filesystem/code/network, 6 financial (exactly the six
  verified `trade_*` execution tools).

## Honest status

| Area | Status |
|---|---|
| MCP client + MT5 data path | **Live-verified** against the real server |
| 106-action manifest + tiers | **Live-verified** (107 actions classified) |
| Dispatcher gate chain | Mock-verified (mocks reproduce live wire behavior) |
| NSE Core REST/WSS contract | PROPOSED — mock-verified only; must be reconciled against the deployed NSE Core API (`docs/PROPOSED_NSE_PLATFORM_INTEGRATION.md`) |
| Windows GUI, single EXE, installer-free release | Not started (next phase) |
| Secret storage | Windows Credential Manager round-trip tested on this machine |
