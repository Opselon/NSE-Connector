# NSE Connector — Gap Analysis (directive §1)

**Date:** 2026-11-17 · **Baseline:** commit `29d4f30` + live probes 2026-11-16/17
**Method:** repository inspection + three live discovery probes against the
customer MT5 MCP server (`http://127.0.0.1:22346/mcp`, MetaTrader 5 MCP v1.0,
terminal build 6306, MetaQuotes-Demo demo account) + first E2E evidence
batches (`docs/e2e-evidence/`, 32 records).

---

## 1. Action-count reconciliation — 136 (directive) vs 107 (live)

| Probe | Date | Count |
|---|---|---|
| Initial discovery | 2026-11-16 | 106 |
| Re-discovery | 2026-11-16 | 107 |
| Directive-check re-discovery | 2026-11-17 | **107** |

The live MCP server exposes **107 actions**; the directive states 136. Per
the directive's own law ("do not invent missing actions or schemas"), the
 Connector claims **107 discovered actions**, reports
`fewer_than_expected (delta −29)` against the stated 136, and reconciles the
136-item P0/P1 checklist as a **capability checklist** — most items are
field-level capabilities that map onto shared actions (e.g. checklist items
2–11 all map to `get_trading_account_info` fields). The full mapping is in
[CAPABILITY_MATRIX.md](CAPABILITY_MATRIX.md). Nothing was invented to close
the count gap.

## 2. Capability gaps in the actual manifest (checklist items with no direct action)

| Checklist item(s) | Status | Evidence-based finding |
|---|---|---|
| 7 — account leverage | **Not available as a field** | Absent from `get_trading_account_info` AND `terminal_get_info` (E001/E006). **Derivable**: `trade_order_calc_margin` → leverage = `contract_size × price ÷ margin` (E103: 100000×1.10033÷110.03 ≈ 1000:1). |
| 12 — *all* available symbols | **Covered — richer than expected** | `get_marketwatch_symbols{include_hidden:true}` returned **4096 symbols with full specifications** (E101). The name misleads: it is the complete symbol inventory. |
| 13, 104–108 — symbol specs | **Covered** | Same tool: `volume_min/max/step`, `tick_size`, `tick_value`, `digits`, `point`, `contract_size`, `trade_filling_mode`, `trade_order_mode`, `trade_exemode`, `trade_stops_level`, `trade_freeze_level`, `swap_*`, `margin_initial/maintenance`, `calculation_mode`. Broker-compat layer (§7) has its dataset. |
| 65–67 — per-deal commission/swap/fee | **Partially available** | Verified deal records carry **no** `commission`/`swap`/`fee` fields (E003 — MetaQuotes demo). Account-level `commissions`/`swaps` aggregates exist. Broker-dependent; Platform must handle absence. |
| 73 — SL/TP in historical trades | **Not available** | Verified order/deal records carry no SL/TP fields; live SL/TP only on open positions (E002). |
| 102 — symbol trading sessions | **Partially available** | `swap_rollover3day` + trade modes present; explicit session times not in the record. |
| 98–100 — live tick subscription | **Mechanics verified, content market-gated** | `market_book_subscribe → market_book_get → market_book_unsubscribe` verified live (E110–E114). Book returned `total_levels: 0` — market was closed (Sunday). Retry at market open. |
| 23/24 — per-ticket detail lookup | **Derived** | No by-ticket tool; filter from position/order lists (Connector-side derivation). |

## 3. Wire behaviors discovered by E2E that the implementation had to absorb

1. **Mutation tools answer plain text, not JSON** (E100:
   `add_marketwatch_symbol` → `"market watch symbol added"`, `isError:false`).
   The dispatcher originally mis-classified these as failures — **fixed**
   (`ToolOutcome::Text`, recorded as `{"kind":"text","text":…}` in results,
   tested).
2. **Symbol-prerequisite chain**: `market_book_get` requires
   `market_book_subscribe` first (E008); calc/trade tools require the symbol
   in Market Watch (E011: "symbol not found in market watch"). The Connector
   must orchestrate `add_marketwatch_symbol` → operation → cleanup.
3. **Subscription lifecycle is not idempotent**: double
   `market_book_unsubscribe` errors (E113); `market_book_get` after
   unsubscribe errors (E114). Error-catalog entries.
4. **Market-closed is a distinct error**: `trade_check_market_order` →
   "trading is not permitted" while `mcp_trade_allowed: true` — Sunday, FX
   market closed (E105–E108). Checklist 117 captured; retry strategy =
   wait for market open, re-validate before any execution.
5. **Guarded terminal state**: `remove_marketwatch_symbol` refuses symbols
   used by open positions/charts/EAs (E199) — safe-state design.
6. **Zero-valued spec fields exist**: EURUSD reports `tick_value: 0.0`,
   `tick_size: 0.0` (E101) — broker variations must be tolerated, values
   derived via calc tools when absent.

## 4. Implementation status vs the directive

| Area | Status |
|---|---|
| MCP client (auth/session/discovery/recovery) | **Live-verified** (10 live tests + E2E evidence) |
| 4 trading data tools + models | **Live-verified** field names (`position_id`/`order_id`/`deal_id`, no-timezone timestamps) |
| Manifest + Tier 0–3 classification | **Live-verified** over all 107 actions |
| Universal dispatcher gate chain | Mock-verified + Tier-0 live dispatch |
| Sync engine (dedup/checkpoint/acks/bounded queues) | Mock-verified; E2E loop with a live Platform **pending** (no reachable Platform API) |
| NSE Core REST/WSS contract | **PROPOSED ONLY** — `github.com/Opselon/NSE-Platform` unreachable (TLS timeout); mock-verified, never claim real integration |
| Per-action E2E evidence | **P0 read block done (32 records)**; trading ops (26–60) pending market open; non-trading actions (charts/files/tester/etc.) pending |
| Error classification catalog (§4) | Seeded from E2E (see [ERROR_CATALOG.md](ERROR_CATALOG.md)); full 107-action sweep pending |
| Performance measurements (§5) | **Not done** — latency captured per evidence record (1–85 ms) but no sustained throughput/RAM/CPU report yet |
| Windows GUI / single EXE | Not started |
| CI on GitHub | Not configured yet |

## 5. Corrections to prior work (honesty ledger)

- Earlier "representative" position/deal fixtures were **guesses**; live
  probing replaced them with verified field names + an alias layer.
- `EXPECTED_ACTION_COUNT` was 106 (first probe), now 136 per directive with
  the discrepancy documented rather than hidden.
- The dispatcher's JSON-only assumption was wrong for mutation tools —
  fixed with `ToolOutcome`.
- Schema validation covers the **subset** of JSON Schema the server emits
  (`type/properties/required/additionalProperties`); documented limitation.

## 6. Immediate next steps (ordered by directive §3)

1. **Trading-ops E2E (checklist 26–60)** on the demo account at market open
   (after ~22:00 UTC Sunday / Monday session): market buy → verify ticket/
   price/volume → modify SL/TP → partial close → full close → pending
   limit/stop lifecycle → delete → verify history reflection.
2. Error catalog sweep across all Tier 1/2 actions with invalid parameters.
3. Non-trading action E2E (charts/files/journals/calendar/news/tester) with
   disposable test artifacts only.
4. Performance report (§5): sustained polling latency, RAM/CPU idle,
   reconnect recovery time.
5. CI workflow on the repository; then Platform reconciliation as soon as
   the NSE-Platform repo/API is reachable.
