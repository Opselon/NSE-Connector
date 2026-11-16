# Capability Matrix — 136-item directive checklist ↔ 107 discovered actions

**Legend:** `E2E-verified` = live MT5 evidence · `E2E-partial` = mechanics verified, content market-gated · `mock-verified` = connector mechanics proven on mocks · `derived` = computed from verified inputs · `live-pending` = test written, execution pending (market-closed / Platform unreachable) · `not-available` = absent from live MCP payloads.

**Status distribution:** E2E-verified: 46, E2E-partial: 15, mock-verified: 16, derived: 16, live-pending: 37, not-available: 6

Evidence records: `docs/e2e-evidence/*.json` (sanitized, latency measured).


## P0 — Account & Trading Data (1-25)

| # | Capability | MCP action(s) | Status | Evidence / notes |
|---|---|---|---|---|
| 1 | Account information | `get_trading_account_info` | **E2E-verified** | E001 — full field list captured |
| 2 | Account balance | `get_trading_account_info` | **E2E-verified** | E001 (balance=26237.73 USD) |
| 3 | Account equity | `get_trading_account_info` | **E2E-verified** | E001 (equity field) |
| 4 | Free margin | `get_trading_account_info` | **E2E-verified** | E001 (margin_free) |
| 5 | Used margin | `get_trading_account_info` | **E2E-verified** | E001 (margin) |
| 6 | Margin level | `get_trading_account_info` | **derived** | derive: equity/margin*100 (fields verified E001) |
| 7 | Account leverage | `get_time_information` | **not-available** | field absent from account AND terminal payload (E001/E006); DERIVABLE via E103 margin calc: 100000*1.10033/110.03 ≈ 1000:1 |
| 8 | Account currency | `get_trading_account_info` | **E2E-verified** | E001 (currency=USD) |
| 9 | Account trading permissions | `get_trading_account_info / terminal_get_info` | **E2E-verified** | E001 read_only/type + E006 mcp_trade_allowed/experts_trade_allowed |
| 10 | Account connection status | `terminal_get_info` | **E2E-verified** | E006 connection.state=synchronized |
| 11 | Account/server/broker info | `get_trading_account_info / terminal_get_info` | **E2E-verified** | E001/E006 |
| 12 | All available trading symbols | `get_marketwatch_symbols` | **E2E-verified** | E101 — 4096 symbols with include_hidden:true |
| 13 | Symbol specifications | `get_marketwatch_symbols` | **E2E-verified** | E101 — 38 spec fields per symbol |
| 14 | Symbol tick information | `get_chart_ticks_history / market_book_subscribe/get/unsubscribe` | **E2E-partial** | E010 tick records verified; book content empty (market closed, E111) |
| 15 | Current Bid/Ask | `market_book_subscribe/get/unsubscribe / get_chart_ticks_history` | **E2E-partial** | bid in tick records (E010); book levels empty until market open (E111) |
| 16 | Historical market prices | `get_chart_ticks_history / get_chart_history` | **E2E-verified** | E009/E010 |
| 17 | OHLC candles per timeframe | `get_chart_history` | **E2E-partial** | E009 mechanics verified; field-level check + market-open content pending |
| 18 | Current open positions | `get_trading_open_positions` | **E2E-verified** | E002 (3 live positions) |
| 19 | All pending orders | `get_trading_open_positions` | **E2E-verified** | E002 orders[] (0 pending — empty verified) |
| 20 | Complete trading history | `get_trading_history_orders` | **E2E-verified** | E003 (7-day range, 3 orders + 3 deals) |
| 21 | Historical deals | `get_trading_history_orders` | **E2E-verified** | E003 deals[] |
| 22 | Historical orders | `get_trading_history_orders` | **E2E-verified** | E003 orders[] |
| 23 | Position details (by ticket) | `get_trading_open_positions` | **derived** | no by-ticket tool; filter from positions list (E002) |
| 24 | Order details (by ticket) | `get_trading_open_positions / get_trading_history_orders` | **derived** | filter from open/history orders |
| 25 | Trade execution details | `get_trading_history_orders` | **E2E-verified** | deals[] carry order_id/position_id links (E003) |

## P0 — Position Management (26-45)

| # | Capability | MCP action(s) | Status | Evidence / notes |
|---|---|---|---|---|
| 26 | Open market BUY | `trade_send_market_order` | **live-pending** | demo-account E2E at market open (blocked Sunday) |
| 27 | Open market SELL | `trade_send_market_order` | **live-pending** | same |
| 28 | Close BUY position | `trade_close_single_position` | **live-pending** | same |
| 29 | Close SELL position | `trade_close_single_position` | **live-pending** | same |
| 30 | Partial close | `trade_close_single_position` | **live-pending** | verify volume parameter semantics live |
| 31 | Full close | `trade_close_single_position` | **live-pending** | same |
| 32 | Modify position SL | `trade_modify_sl_tp` | **live-pending** | market-open E2E |
| 33 | Modify position TP | `trade_modify_sl_tp` | **live-pending** | same |
| 34 | Modify SL and TP | `trade_modify_sl_tp` | **live-pending** | same |
| 35 | Remove SL | `trade_modify_sl_tp` | **live-pending** | verify removal semantics (null vs 0) live |
| 36 | Remove TP | `trade_modify_sl_tp` | **live-pending** | same |
| 37 | Modify position (other) | `trade_modify_sl_tp` | **not-available** | MT5 does not support modifying open-position volume/price — only SL/TP (by design) |
| 38 | Result of modification | `trade_modify_sl_tp → read-back` | **live-pending** | execute + verify via GP read-back |
| 39 | Verify volume after execution | `trade_send_market_order → get_trading_open_positions` | **live-pending** | follow-up read pattern |
| 40 | Verify execution price | `trade_send_market_order → deals` | **live-pending** | verify via history deals after close |
| 41 | Verify ticket/identifiers | `trade_send_market_order → get_trading_open_positions` | **live-pending** | position_id returned by read |
| 42 | Detect manually opened positions | `sync diff on get_trading_open_positions` | **mock-verified** | sync engine diff; live-poll E2E pending |
| 43 | Detect manually closed positions | `sync diff on get_trading_history_positions` | **mock-verified** | same |
| 44 | Detect externally modified positions | `sync diff SL/TP` | **mock-verified** | same |
| 45 | Synchronize position changes | `sync engine` | **mock-verified** | bounded batches + checkpoint verified on mock; full live loop needs Platform |

## P0 — Pending Orders (46-60)

| # | Capability | MCP action(s) | Status | Evidence / notes |
|---|---|---|---|---|
| 46 | Place Buy Limit | `trade_send_pending_order` | **live-pending** | order_mode bitmask 127 (E101) includes limit/stop/stop-limit |
| 47 | Place Sell Limit | `trade_send_pending_order` | **live-pending** | same |
| 48 | Place Buy Stop | `trade_send_pending_order` | **live-pending** | same |
| 49 | Place Sell Stop | `trade_send_pending_order` | **live-pending** | same |
| 50 | Place Buy Stop Limit | `trade_send_pending_order` | **live-pending** | supported per order_mode; verify schema/enum |
| 51 | Place Sell Stop Limit | `trade_send_pending_order` | **live-pending** | same |
| 52 | Modify pending order price | `possible gap` | **live-pending** | no dedicated tool; verify whether trade_modify_sl_tp covers orders — schema check required |
| 53 | Modify pending order volume | `possible gap` | **live-pending** | same |
| 54 | Modify pending order SL | `trade_modify_sl_tp` | **live-pending** | verify order ticket accepted |
| 55 | Modify pending order TP | `trade_modify_sl_tp` | **live-pending** | same |
| 56 | Modify pending order expiration | `possible gap` | **live-pending** | no dedicated tool; schema check required |
| 57 | Delete pending order | `trade_delete_order` | **live-pending** | market-open E2E |
| 58 | Pending order execution status | `get_trading_history_orders (state)` | **E2E-verified** | state field verified (filled/placed) E003 |
| 59 | Detect triggered pending orders | `sync diff on get_trading_open_positions + get_trading_history_orders` | **mock-verified** | order disappears + position appears |
| 60 | Triggered order → position conversion | `get_trading_history_orders + deals` | **E2E-partial** | order_id/position_id linkage verified (E003); trigger event E2E pending |

## P0 — Trading History & Synchronization (61-79)

| # | Capability | MCP action(s) | Status | Evidence / notes |
|---|---|---|---|---|
| 61 | History for date range | `get_trading_history_orders` | **E2E-verified** | E003 range params verified; inverted-range error captured (E906) |
| 62 | Today's history | `get_trading_history_orders` | **derived** | same tool, range=today |
| 63 | Historical closed positions | `get_trading_history_positions` | **E2E-verified** | E004 (empty on fresh demo — shape verified) |
| 64 | Deals and executions | `get_trading_history_orders` | **E2E-verified** | E003 |
| 65 | Commissions | `deals fields` | **not-available** | verified: NO commission field in deal records (E003, MetaQuotes demo); account-level `commissions` aggregate only |
| 66 | Swap charges | `deals fields` | **not-available** | same — per-deal swap absent; account-level swaps exists |
| 67 | Trading fees | `deals fields` | **not-available** | same — fee field absent |
| 68 | Realized P/L | `get_trading_history_orders deals` | **derived** | sum(entry=out).profit (fields verified E003) |
| 69 | Unrealized P/L | `get_trading_open_positions (profit)` | **E2E-verified** | E002 per-position profit |
| 70 | Open/close timestamps | `get_trading_history_orders / get_trading_history_positions` | **E2E-verified** | open_time/done_time verified (E003); no-timezone format documented |
| 71 | Direction and volume | `get_trading_history_orders` | **E2E-verified** | E003 |
| 72 | Entry and exit prices | `get_trading_history_orders` | **E2E-verified** | price_open per deal (E003) |
| 73 | SL/TP on historical trades | `deals fields` | **not-available** | verified absent from order/deal records (E003) |
| 74 | Detect new historical records | `sync engine dedup` | **mock-verified** | DedupSet verified |
| 75 | Detect account state changes | `sync diff on get_trading_account_info` | **mock-verified** | snapshot diffing |
| 76 | Incremental updates | `sync checkpoint` | **mock-verified** | checkpoint verified |
| 77 | Recovery after disconnection | `sync engine` | **mock-verified** | gap-fill range verified; live-interruption E2E pending |
| 78 | Prevent duplicate records | `sync engine` | **mock-verified** | duplicate batch dedup verified |
| 79 | MT5 ↔ server consistency | `Platform` | **live-pending** | requires live Platform API (unreachable) |

## P0 — Risk Management & Calculations (80-97)

| # | Capability | MCP action(s) | Status | Evidence / notes |
|---|---|---|---|---|
| 80 | Calculate position volume | `Platform-side formula` | **derived** | equity×risk%÷(SL distance×pip value); inputs verified (E001/E101/E104) |
| 81 | Monetary risk | `derived` | **derived** | volume×SL distance×pip value |
| 82 | Risk % of equity | `derived` | **derived** | from E001 equity |
| 83 | SL distance | `derived` | **derived** | open price vs SL |
| 84 | TP distance | `derived` | **derived** | open price vs TP |
| 85 | Risk/Reward ratio | `derived` | **derived** | 83/84 |
| 86 | Potential profit | `trade_order_calc_profit` | **E2E-verified** | E104: +10 pips × 0.1 lot EURUSD = +10.00 USD |
| 87 | Potential loss | `trade_order_calc_profit` | **E2E-verified** | same tool, negative side (E104 mechanics) |
| 88 | Pip/point value | `trade_order_calc_profit` | **E2E-verified** | E104: 10 USD / 10 pips / 0.1 lot = 1 USD per pip — matches EURUSD math |
| 89 | Tick value | `get_marketwatch_symbols` | **E2E-partial** | spec field exists but EURUSD reports 0.0 (E101) — derive via CP when zero |
| 90 | Required margin | `trade_order_calc_margin` | **E2E-verified** | E103: 110.03 USD for 0.1 lot EURUSD at live price |
| 91 | Available margin after trade | `get_trading_account_info + trade_order_calc_margin` | **derived** | margin_free − calc_margin (both verified) |
| 92 | Aggregate open exposure | `get_trading_open_positions` | **derived** | sum position volumes × contract_size (E002/E101) |
| 93 | Total account risk | `derived` | **derived** | from 81-85 across positions |
| 94 | Floating P/L | `get_trading_account_info (profit)` | **E2E-verified** | E001 + per-position E002 |
| 95 | Daily realized P/L | `get_trading_history_orders deals` | **derived** | sum today's out-deals |
| 96 | Daily total P/L | `derived` | **derived** | 94 + 95 |
| 97 | Cross-symbol verification | `trade_order_calc_margin / trade_order_calc_profit` | **live-pending** | run matrix across symbol set (XAUUSD, indices) |

## P1 — Market Data & Analysis Inputs (98-110)

| # | Capability | MCP action(s) | Status | Evidence / notes |
|---|---|---|---|---|
| 98 | Subscribe to live ticks | `market_book_subscribe/get/unsubscribe` | **E2E-partial** | E110-E114: subscribe/get/unsubscribe verified; book EMPTY while market closed; no push notifications in MCP (poll required) |
| 99 | Receive price changes | `market_book_subscribe/get/unsubscribe / get_chart_ticks_history` | **E2E-partial** | content requires market open |
| 100 | Receive candle updates | `get_chart_history polling` | **live-pending** | no MCP push; controlled polling design |
| 101 | Historical candles across timeframes | `get_chart_history` | **E2E-partial** | E009 M15 verified; period enum covers M1..MN1 — cross-timeframe sweep pending |
| 102 | Symbol trading sessions | `get_marketwatch_symbols` | **E2E-partial** | swap_rollover3day + trade modes present; explicit session times absent (E101) |
| 103 | Contract specifications | `get_marketwatch_symbols` | **E2E-verified** | E101 contract_size/currencies/calculation_mode |
| 104 | Min/max lot size | `get_marketwatch_symbols` | **E2E-verified** | E101 volume_min=0.01 / volume_max=500 (EURUSD) |
| 105 | Lot step | `get_marketwatch_symbols` | **E2E-verified** | E101 volume_step=0.01 |
| 106 | Tick size and tick value | `get_marketwatch_symbols` | **E2E-partial** | fields present; EURUSD values 0.0 (broker variation) — derive when zero |
| 107 | Digits and point size | `get_marketwatch_symbols` | **E2E-verified** | E101 digits=5, point=1e-05 |
| 108 | Execution and filling modes | `get_marketwatch_symbols` | **E2E-verified** | E101 trade_exemode/trade_filling_mode/trade_order_mode/trade_mode |
| 109 | Spread information | `get_marketwatch_symbols / market_book_subscribe/get/unsubscribe` | **E2E-partial** | spread_float flag verified; numeric spread requires market open |
| 110 | Market availability and restrictions | `get_marketwatch_symbols / trade_check_market_order` | **E2E-partial** | trade_mode field + 'trading is not permitted' captured (E105) |

## P1 — Execution Results & Error Handling (111-125)

| # | Capability | MCP action(s) | Status | Evidence / notes |
|---|---|---|---|---|
| 111 | Successful order execution | `trade_send_market_order` | **live-pending** | trading-op E2E at market open |
| 112 | Rejected order responses | `trade_check_market_order` | **E2E-partial** | rejection path captured (market-closed variant, E105); broker-rejection variants at market open |
| 113 | Insufficient margin errors | `trade_check_market_order / trade_order_calc_margin` | **live-pending** | construct oversized order at market open |
| 114 | Invalid volume errors | `trade_check_market_order / trade_order_calc_margin` | **E2E-verified** | E902 'volume must be greater than 0'; E106 excessive volume |
| 115 | Invalid price errors | `trade_send_pending_order` | **live-pending** | pending-order E2E |
| 116 | Invalid SL/TP errors | `trade_check_market_order` | **live-pending** | E107 attempt hit market-closed first — retry at open |
| 117 | Market closed responses | `trade_check_market_order` | **E2E-verified** | E105-E108 'trading is not permitted' with all trade flags true → market closed (Sunday) |
| 118 | Unsupported filling mode | `trade_send_pending_order` | **live-pending** | filling_mode=1 spec + live probe at open |
| 119 | Invalid/unavailable symbols | `trade_check_market_order / trade_order_calc_margin` | **E2E-verified** | E901/E108 INVALID_SYM_XY |
| 120 | Trade-disabled responses | `trade_check_market_order` | **E2E-partial** | read_only=false account verified; true-disabled case needs a trade-disabled terminal |
| 121 | Timeout during execution | `connector` | **mock-verified** | timeout enforcement verified (mock hang) |
| 122 | Connection interruption | `connector` | **mock-verified** | session-loss recovery verified live + mock reconnect |
| 123 | Duplicate command prevention | `dispatcher` | **mock-verified** | replay cache verified |
| 124 | Partial execution responses | `trade_send_market_order` | **live-pending** | partial close E2E |
| 125 | Execution reconciliation after reconnect | `sync engine` | **mock-verified** | checkpoint + dedup |

## P1 — Connector & MCP Lifecycle (126-136)

| # | Capability | MCP action(s) | Status | Evidence / notes |
|---|---|---|---|---|
| 126 | Discover all MCP tools | `tools/list` | **E2E-verified** | 107 actions, 3 probes (docs/GAP_ANALYSIS.md §1) |
| 127 | Complete tool descriptions | `tools/list` | **E2E-verified** | captured for all actions |
| 128 | Input schemas | `tools/list` | **E2E-verified** | captured; drive E2E arg construction |
| 129 | Validate parameters against schemas | `schema validator` | **E2E-verified** | subset validator + E2E schema-driven args (E001-E013) |
| 130 | Generic dispatcher execution | `dispatcher` | **E2E-partial** | Tier-0 live dispatch verified; per-action sweep pending |
| 131 | Validate and serialize responses | `mapping layer` | **E2E-verified** | live-verified field names + alias layer |
| 132 | MCP errors and protocol failures | `client` | **E2E-verified** | -32602 (E908), 401/404 live, malformed bodies (mock) |
| 133 | Connection loss and reconnection | `client` | **E2E-verified** | 404 session-loss live test + one-shot re-init + mock reconnect loop |
| 134 | Correlation and acknowledgment | `contract` | **mock-verified** | message_id/correlation_id verified; live acks need Platform |
| 135 | Cancellation and timeout | `connector` | **mock-verified** | timeout verified; cancellation via shutdown channels |
| 136 | All remaining discovered actions | `charts/files/journals/calendar/news/tester/profiles/services` | **live-pending** | 67 non-trading actions; read-only sweep next |

---

**Count reconciliation:** the directive states 136 actions; the live server exposes 107. This matrix maps the 136 *capability items* onto those 107 *actions* (multiple items share one action or are derived from its fields). No actions were invented; see GAP_ANALYSIS.md §1.
