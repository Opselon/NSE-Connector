# NSE MT5 Bridge — 90-command surface (v1)

The bridge exposes **90 platform-facing commands** in 10 categories. The
Platform speaks ONLY this normalized JSON API — it never needs to know which
broker sits behind the connector.

Every command — whether it arrives as a platform `command` frame over WS or
is executed by the daemon — answers the **same JSON envelope**:

```json
{
  "command": "get_account_balance",
  "ok": true,
  "data": { "balance": 26237.73, "credit": 0.0, "currency": "USD" },
  "broker_time": "2026-11-17T15:43:45",
  "error": null
}
```

Failures are truthful, never fabricated:

```json
{
  "command": "get_chart_indicators",
  "ok": false,
  "data": null,
  "broker_time": "2026-11-17T15:43:45",
  "error": {
    "code": "not_available",
    "message": "get_chart_indicators is not exposed by this terminal (terminal exposes no indicator introspection)"
  }
}
```

Error codes: `invalid_params` · `not_available` · `mt5_error` · `bridge_internal`.

**Tier model** (identical vocabulary to the action dispatcher):
- `read_only` — data reads, calculations, bridge status. No permission needed.
- `local_modification` — affects local terminal state (e.g. `get_market_depth`
  internally subscribes the level-2 book). Needs `local.modify`.
- `financial` — order execution. Needs `trading.execute` **and** explicit
  financial consent, blocked instantly by any engaged kill switch.

Broker-dependent fields that the broker does not expose (verified live:
no leverage field, no per-deal commission history, tick_value sometimes 0.0)
are answered `not_available` with the reason, or derived through the broker's
own calculators with every intermediate value exposed.

## Command registry

The authoritative live list is `get_bridge_capabilities` (returns this table
plus the discovered MCP action count and manifest hash). The full 90:

| # | Command | Category | Tier | Implementation |
|---|---------|----------|------|----------------|
| 1 | `get_bridge_status` | terminal | RO | bridge state |
| 2 | `get_mt5_connection_status` | terminal | RO | state + live terminal probe |
| 3 | `connect_mt5` | terminal | RO | MCP handshake |
| 4 | `disconnect_mt5` | terminal | RO | release MCP session |
| 5 | `reconnect_mt5` | terminal | RO | MCP re-handshake |
| 6 | `get_mt5_terminal_info` | terminal | RO | `terminal_get_info` |
| 7 | `get_mt5_terminal_version` | terminal | RO | derive (build, program) |
| 8 | `get_mt5_server_info` | terminal | RO | derive (server, broker) |
| 9 | `get_mcp_server_info` | terminal | RO | handshake + `tools/list` |
| 10 | `get_trading_account_info` | account | RO | `get_trading_account_info` |
| 11 | `get_account_balance` | account | RO | derive |
| 12 | `get_account_equity` | account | RO | derive |
| 13 | `get_account_margin` | account | RO | derive |
| 14 | `get_account_free_margin` | account | RO | derive |
| 15 | `get_account_margin_level` | account | RO | **derived** (no field live) |
| 16 | `get_account_profit` | account | RO | derive (+ swaps, commissions) |
| 17 | `get_account_currency` | account | RO | derive |
| 18 | `get_account_trading_permissions` | account | RO | terminal flags + account type |
| 19 | `get_trading_symbols` | symbols | RO | `get_marketwatch_symbols` |
| 20 | `get_symbol_info` | symbols | RO | full 38-field spec |
| 21 | `get_symbol_price` | symbols | RO | last tick |
| 22 | `get_symbol_tick_size` | symbols | RO | spec (derives from point when 0.0) |
| 23 | `get_symbol_point` | symbols | RO | spec |
| 24 | `get_symbol_digits` | symbols | RO | spec |
| 25 | `get_symbol_spread` | symbols | RO | derive (price + points) |
| 26 | `get_symbol_trading_sessions` | symbols | RO | spec fields or `not_available` |
| 27 | `get_symbol_trade_constraints` | symbols | RO | spec (volume/trade/order modes) |
| 28 | `get_market_tick` | market | RO | tick history limit 1 |
| 29 | `get_market_bid` | market | RO | derive |
| 30 | `get_market_ask` | market | RO | derive |
| 31 | `get_market_last_price` | market | RO | derive (note: FX has no last) |
| 32 | `get_market_depth` | market | local | book subscribe + `market_book_get` |
| 33 | `get_market_spread_history` | market | RO | **computed series** from ticks |
| 34 | `get_market_tick_volume` | market | RO | ticks (or `not_available`) |
| 35 | `get_market_watchlist` | market | RO | Market Watch selected |
| 36 | `subscribe_market_ticks` | market | RO | poll registration (mode: poll) |
| 37 | `get_chart_history` | chart | RO | `get_chart_history` |
| 38 | `get_chart_ticks_history` | chart | RO | `get_chart_ticks_history` |
| 39 | `get_chart_current_timeframe` | chart | RO | manifest check → call or `not_available` |
| 40 | `get_chart_available_timeframes` | chart | RO | static MT5 set |
| 41 | `get_chart_ohlcv` | chart | RO | normalized OHLCV |
| 42 | `get_chart_indicators` | chart | RO | manifest check → or `not_available` |
| 43 | `get_chart_objects` | chart | RO | manifest check → or `not_available` |
| 44 | `get_chart_active_info` | chart | RO | manifest check → or `not_available` |
| 45 | `get_chart_multi_timeframe_history` | chart | RO | fan-out ≤5 timeframes |
| 46 | `get_trading_open_positions` | positions | RO | `get_trading_open_positions` |
| 47 | `get_position_info` | positions | RO | find by ticket |
| 48 | `get_position_profit` | positions | RO | derive |
| 49 | `get_position_volume` | positions | RO | derive |
| 50 | `get_position_sl_tp` | positions | RO | derive |
| 51 | `get_position_risk` | positions | RO | **derived** (margin + SL/TP outcomes) |
| 52 | `trade_modify_sl_tp` | positions | FIN | `trade_modify_sl_tp` |
| 53 | `trade_close_single_position` | positions | FIN | `trade_close_single_position` |
| 54 | `trade_close_all_positions` | positions | FIN | **composite** (≤50, per-step results) |
| 55 | `get_trading_pending_orders` | orders | RO | open positions incl. orders |
| 56 | `get_order_info` | orders | RO | working + recent history lookup |
| 57 | `get_order_status` | orders | RO | derive (state) |
| 58 | `get_trading_history_orders` | orders | RO | `get_trading_history_orders` |
| 59 | `get_trading_history_positions` | orders | RO | `get_trading_history_positions` |
| 60 | `get_trading_deals_history` | orders | RO | history incl. deals |
| 61 | `trade_cancel_pending_order` | orders | FIN | `trade_delete_order` |
| 62 | `trade_modify_pending_order` | orders | FIN | SL/TP only — price modify `not_available` (schema) |
| 63 | `get_order_execution_details` | orders | RO | deals filtered by order_id |
| 64 | `trade_send_market_order` | trading | FIN | `trade_send_market_order` |
| 65 | `trade_send_buy_order` | trading | FIN | wrapper (type=buy) |
| 66 | `trade_send_sell_order` | trading | FIN | wrapper (type=sell) |
| 67 | `trade_send_pending_order` | trading | FIN | `trade_send_pending_order` |
| 68 | `trade_close_partial_position` | trading | FIN | `not_available` (no volume param in MCP schema) |
| 69 | `trade_reverse_position` | trading | FIN | **composite**: close + opposite open |
| 70 | `trade_set_position_sl` | trading | FIN | modify wrapper |
| 71 | `trade_set_position_tp` | trading | FIN | modify wrapper |
| 72 | `trade_calculate_order_result` | trading | RO | margin + profit at TP and SL |
| 73 | `calculate_position_size` | risk | RO | **derived** (risk budget ÷ SL·pip value) |
| 74 | `calculate_trade_risk` | risk | RO | **derived** (margin + SL loss + %equity) |
| 75 | `calculate_stop_loss_price` | risk | RO | **derived** (side-aware, digits-rounded) |
| 76 | `calculate_take_profit_price` | risk | RO | **derived** |
| 77 | `calculate_margin_required` | risk | RO | `trade_order_calc_margin` |
| 78 | `calculate_margin_level_after_trade` | risk | RO | **derived** |
| 79 | `calculate_pip_value` | risk | RO | **measured** via profit calc or spec |
| 80 | `calculate_profit_loss` | risk | RO | `trade_order_calc_profit` |
| 81 | `validate_trade_request` | risk | RO | `trade_check_market/pending_order` |
| 82 | `get_mt5_last_error` | diagnostics | RO | bridge state |
| 83 | `get_bridge_logs` | diagnostics | RO | ring buffer (≤200) |
| 84 | `get_bridge_health` | diagnostics | RO | verdict + counters |
| 85 | `get_bridge_latency` | diagnostics | RO | latency stats |
| 86 | `get_mt5_connection_metrics` | diagnostics | RO | reconnects, ping, session age |
| 87 | `get_trade_execution_metrics` | diagnostics | RO | executed/rejected/failed |
| 88 | `get_bridge_capabilities` | diagnostics | RO | this registry, live |
| 89 | `get_mcp_available_tools` | diagnostics | RO | `tools/list` |
| 90 | `get_mt5_diagnostic_report` | diagnostics | RO | composite, degrades per-section |

## Gate chain (bridge commands)

Every platform command frame runs the SAME local gate chain as native
manifest actions, before any handler runs:

```
kill switch (any scope) → expiry → device binding → account binding
→ tier permission → financial consent (Tier 3) → replay protection
→ handler (per-command parameter validation)
```

A rejected command NEVER reaches MT5; the rejection reason travels back in
the `action.result` frame with `status: "rejected"`.

## Wire notes (live-verified)

- Broker ids are strings on the wire; numeric forms are tolerated on read.
- Timestamps inside payloads are the broker's verbatim strings (ISO without
  timezone); `broker_time` in the envelope is the bridge's broker-format UTC.
- Mutation tools may answer plain text — the bridge normalizes their outcome
  into the envelope (`{"kind": "text", "text": ...}` inside `result`).
- `trade_close_partial_position` is honestly `not_available`: the MT5 MCP
  schema has no volume parameter on close, and faking it with an opposite
  order would open a hedged position instead of reducing this one.
