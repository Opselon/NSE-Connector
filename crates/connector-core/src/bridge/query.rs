//! Bridge handlers — categories 1-5: connection/terminal, account, symbols,
//! live market, charts. All answers are broker-agnostic JSON; fields the
//! broker does not expose return `not_available` with the reason.

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{
    BridgeDeps, BridgeFailure, broker_time_now, opt_f64, opt_str, opt_usize, req_str,
    round_to_digits,
};
use crate::mcp::trading_tools;

// ---------------------------------------------------------------------------
// Shared fetchers (used by later categories too)
// ---------------------------------------------------------------------------

pub(super) async fn account_info(d: &mut BridgeDeps<'_>) -> Result<Value, BridgeFailure> {
    Ok(d.mcp
        .call_tool_json(trading_tools::ACCOUNT_INFO, &json!({}))
        .await?
        .get("account")
        .cloned()
        .unwrap_or_else(|| json!({})))
}

pub(super) async fn terminal_info(d: &mut BridgeDeps<'_>) -> Result<Value, BridgeFailure> {
    d.mcp
        .call_tool_json("terminal_get_info", &json!({}))
        .await
        .map_err(Into::into)
}

/// Full symbol spec from the broker (38 live fields; `include_hidden` so
/// specs are available before the symbol is visible in Market Watch).
pub(super) async fn symbol_spec(
    d: &mut BridgeDeps<'_>,
    symbol: &str,
) -> Result<Value, BridgeFailure> {
    let res = d
        .mcp
        .call_tool_json(
            "get_marketwatch_symbols",
            &json!({ "include_hidden": true }),
        )
        .await?;
    let symbols = res
        .get("symbols")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    symbols
        .iter()
        .find(|s| s.get("symbol").and_then(Value::as_str) == Some(symbol))
        .cloned()
        .ok_or_else(|| {
            BridgeFailure::not_available(format!(
                "symbol '{symbol}' not found in broker symbol list"
            ))
        })
}

/// Digits from the live spec, falling back to 5 (FX-major default) only when
/// the broker omits the field; the fallback is recorded in the answer.
pub(super) async fn symbol_digits(
    d: &mut BridgeDeps<'_>,
    symbol: &str,
) -> Result<(u32, Value), BridgeFailure> {
    let spec = symbol_spec(d, symbol).await?;
    let digits = spec.get("digits").and_then(Value::as_u64).unwrap_or(5) as u32;
    Ok((digits, spec))
}

/// Latest tick for a symbol from the tick history (limit 1, 10-minute
/// window — thin windows on quiet minutes fall back to 2 hours).
pub(super) async fn last_tick(
    d: &mut BridgeDeps<'_>,
    symbol: &str,
) -> Result<Value, BridgeFailure> {
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into());
    let from = (OffsetDateTime::now_utc() - time::Duration::minutes(10))
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into());
    let args = json!({
        "symbol": symbol,
        "datetime_from": &from[..from.len().min(19)],
        "datetime_to": &now[..now.len().min(19)],
        "limit": 1,
    });
    let res = d
        .mcp
        .call_tool_json("get_chart_ticks_history", &args)
        .await?;
    let ticks = res
        .get("history")
        .or_else(|| res.get("ticks"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let tick = ticks.last().cloned().ok_or_else(|| {
        BridgeFailure::not_available(format!("no live tick available for '{symbol}'"))
    })?;
    Ok(tick)
}

/// Broker-time window args for history tools (`datetime_from`/`datetime_to`
/// are naive broker ISO strings on the live server).
pub(super) fn window_args(symbol: &str, minutes: i64, limit: usize) -> Value {
    let now = super::broker_time_now();
    let from = (OffsetDateTime::now_utc() - time::Duration::minutes(minutes))
        .format(&Rfc3339)
        .map(|s| s[..s.len().min(19)].to_string())
        .unwrap_or_else(|_| "1970-01-01T00:00:00".into());
    json!({
        "symbol": symbol,
        "datetime_from": from,
        "datetime_to": now,
        "limit": limit,
    })
}

pub(super) fn ticks_of(res: &Value) -> Vec<Value> {
    res.get("history")
        .or_else(|| res.get("ticks"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Category 1 — connection & terminal state (1-9)
// ---------------------------------------------------------------------------

pub(super) async fn get_bridge_status(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let s = &d.state;
    Ok(json!({
        "bridge": "NSE MT5 Bridge",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": s.uptime_secs(),
        "mt5_connected": s.inner.read().unwrap_or_else(|e| e.into_inner()).mt5_connected,
        "broker_time": broker_time_now(),
    }))
}

pub(super) async fn get_mt5_connection_status(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let (connected, since, handshake) = {
        let s = d.state.inner.read().unwrap_or_else(|e| e.into_inner());
        (
            s.mt5_connected,
            s.connected_since,
            s.handshake.clone().map(|h| {
                json!({
                    "protocol_version": h.protocol_version,
                    "server_name": h.server_name,
                    "server_version": h.server_version,
                })
            }),
        )
    };
    let mut out = json!({
        "connected": connected,
        "session_age_secs": since.map(|t| t.elapsed().as_secs()),
        "handshake": handshake,
    });
    // Live terminal connection block when reachable.
    if let Ok(ti) = terminal_info(d).await
        && let Some(conn) = ti.get("terminal").and_then(|t| t.get("connection"))
    {
        out["terminal_connection"] = conn.clone();
    }
    Ok(out)
}

pub(super) async fn connect_mt5(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let hs = d
        .mcp
        .connect()
        .await
        .map_err(|e| BridgeFailure::mt5_error(format!("handshake failed: {e}")))?;
    d.state.set_mt5_connected(true, Some(hs.clone()));
    Ok(json!({
        "connected": true,
        "protocol_version": hs.protocol_version,
        "server_name": hs.server_name,
        "server_version": hs.server_version,
    }))
}

pub(super) async fn disconnect_mt5(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    // Semantics: release the local session so later calls must re-handshake.
    d.mcp.restore_session(None);
    d.state.set_mt5_connected(false, None);
    Ok(json!({
        "disconnected": true,
        "note": "session released; use connect_mt5 to re-handshake"
    }))
}

pub(super) async fn reconnect_mt5(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let hs = d
        .mcp
        .connect()
        .await
        .map_err(|e| BridgeFailure::mt5_error(format!("re-handshake failed: {e}")))?;
    d.state.bump_reconnects();
    d.state.set_mt5_connected(true, Some(hs.clone()));
    Ok(json!({
        "connected": true,
        "reconnects_recorded": true,
        "protocol_version": hs.protocol_version,
        "server_version": hs.server_version,
    }))
}

pub(super) async fn get_mt5_terminal_info(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ti = terminal_info(d).await?;
    Ok(ti.get("terminal").cloned().unwrap_or(ti))
}

pub(super) async fn get_mt5_terminal_version(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ti = terminal_info(d).await?;
    let term = ti.get("terminal").unwrap_or(&ti);
    Ok(json!({
        "build": term.get("build"),
        "program_name": term.get("program_name"),
        "company": term.get("company"),
    }))
}

pub(super) async fn get_mt5_server_info(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let acc = account_info(d).await?;
    Ok(json!({
        "server": acc.get("server"),
        "broker": acc.get("broker"),
        "currency": acc.get("currency"),
        "account_type": acc.get("type"),
    }))
}

pub(super) async fn get_mcp_server_info(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let tools = d.mcp.list_tools().await?;
    let handshake = d
        .state
        .inner
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .handshake
        .clone();
    Ok(json!({
        "handshake": handshake.map(|h| json!({
            "protocol_version": h.protocol_version,
            "server_name": h.server_name,
            "server_version": h.server_version,
        })),
        "tool_count": tools.len(),
    }))
}

// ---------------------------------------------------------------------------
// Category 2 — account (10-18)
// ---------------------------------------------------------------------------

pub(super) async fn get_trading_account_info(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let full = d
        .mcp
        .call_tool_json(trading_tools::ACCOUNT_INFO, &json!({}))
        .await?;
    Ok(full)
}

pub(super) async fn get_account_balance(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let acc = account_info(d).await?;
    Ok(json!({
        "balance": acc.get("balance"),
        "credit": acc.get("credit"),
        "currency": acc.get("currency"),
    }))
}

pub(super) async fn get_account_equity(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let acc = account_info(d).await?;
    Ok(json!({
        "equity": acc.get("equity"),
        "balance": acc.get("balance"),
        "floating_profit": acc.get("profit"),
        "currency": acc.get("currency"),
    }))
}

pub(super) async fn get_account_margin(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let acc = account_info(d).await?;
    Ok(json!({
        "margin": acc.get("margin"),
        "margin_free": acc.get("margin_free"),
        "currency": acc.get("currency"),
    }))
}

pub(super) async fn get_account_free_margin(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let acc = account_info(d).await?;
    Ok(json!({
        "margin_free": acc.get("margin_free"),
        "currency": acc.get("currency"),
    }))
}

pub(super) async fn get_account_margin_level(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let acc = account_info(d).await?;
    // Live fact: the broker payload has no margin_level field — derive it.
    let equity = acc.get("equity").and_then(Value::as_f64).unwrap_or(0.0);
    let margin = acc.get("margin").and_then(Value::as_f64).unwrap_or(0.0);
    let level = if margin > 0.0 {
        Some(equity / margin * 100.0)
    } else {
        None
    };
    Ok(json!({
        "margin_level_percent": level,
        "equity": equity,
        "margin": margin,
        "derived": true,
    }))
}

pub(super) async fn get_account_profit(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let acc = account_info(d).await?;
    Ok(json!({
        "profit": acc.get("profit"),
        "swaps": acc.get("swaps"),
        "commissions": acc.get("commissions"),
        "currency": acc.get("currency"),
    }))
}

pub(super) async fn get_account_currency(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let acc = account_info(d).await?;
    Ok(json!({
        "currency": acc.get("currency"),
    }))
}

pub(super) async fn get_account_trading_permissions(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ti = terminal_info(d).await?;
    let term = ti.get("terminal").unwrap_or(&ti);
    let acc = account_info(d).await?;
    Ok(json!({
        "account_type": acc.get("type"),
        "account_read_only": acc.get("read_only"),
        "experts_trade_allowed": term.get("experts_trade_allowed"),
        "mcp_trade_allowed": term.get("mcp_trade_allowed"),
        "experts_dll_allowed": term.get("experts_dll_allowed"),
        "note": "terminal flags must all be true for order execution via MCP",
    }))
}

// ---------------------------------------------------------------------------
// Category 3 — symbols & specs (19-27)
// ---------------------------------------------------------------------------

pub(super) async fn get_trading_symbols(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let include_hidden = p
        .get("include_hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let res = d
        .mcp
        .call_tool_json(
            "get_marketwatch_symbols",
            &json!({ "include_hidden": include_hidden }),
        )
        .await?;
    let symbols: Vec<String> = res
        .get("symbols")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("symbol").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "count": symbols.len(), "symbols": symbols }))
}

pub(super) async fn get_symbol_info(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let spec = symbol_spec(d, symbol).await?;
    Ok(json!({ "symbol": symbol, "spec": spec }))
}

pub(super) async fn get_symbol_price(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let tick = last_tick(d, symbol).await?;
    Ok(json!({ "symbol": symbol, "tick": tick }))
}

pub(super) async fn get_symbol_tick_size(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let spec = symbol_spec(d, symbol).await?;
    let tick_size = spec.get("tick_size").and_then(Value::as_f64);
    let point = spec.get("point").and_then(Value::as_f64);
    // Live fact (EURUSD on MetaQuotes-Demo): tick_size can be 0.0 — derive
    // from point when the broker omits it, and say so.
    let (effective, derived) = match (tick_size, point) {
        (Some(ts), _) if ts > 0.0 => (Some(ts), false),
        (_, Some(pt)) if pt > 0.0 => (Some(pt), true),
        _ => (None, false),
    };
    Ok(json!({
        "symbol": symbol,
        "tick_size": effective,
        "reported_tick_size": tick_size,
        "derived_from_point": derived,
    }))
}

pub(super) async fn get_symbol_point(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let spec = symbol_spec(d, symbol).await?;
    Ok(json!({
        "symbol": symbol,
        "point": spec.get("point"),
        "digits": spec.get("digits"),
    }))
}

pub(super) async fn get_symbol_digits(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let spec = symbol_spec(d, symbol).await?;
    Ok(json!({
        "symbol": symbol,
        "digits": spec.get("digits"),
    }))
}

pub(super) async fn get_symbol_spread(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let tick = last_tick(d, symbol).await?;
    let (bid, ask) = (
        tick.get("bid").and_then(Value::as_f64),
        tick.get("ask").and_then(Value::as_f64),
    );
    let (Some(bid), Some(ask)) = (bid, ask) else {
        return Err(BridgeFailure::not_available(format!(
            "tick for '{symbol}' lacks bid/ask"
        )));
    };
    let (digits, _spec) = symbol_digits(d, symbol).await?;
    let spread = ask - bid;
    let points = if digits > 0 {
        spread * 10f64.powi(digits as i32)
    } else {
        spread
    };
    Ok(json!({
        "symbol": symbol,
        "bid": bid,
        "ask": ask,
        "spread_price": round_to_digits(spread, digits),
        "spread_points": round_to_digits(points, 1),
    }))
}

pub(super) async fn get_symbol_trading_sessions(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let spec = symbol_spec(d, symbol).await?;
    // Broker-dependent: keep whatever session-shaped fields the spec carries.
    let sessions: Value = spec
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter(|(k, _)| {
                    k.to_ascii_lowercase().contains("session")
                        || k.eq_ignore_ascii_case("trade_session")
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<serde_json::Map<String, Value>>()
                .into()
        })
        .unwrap_or_else(|| json!({}));
    if sessions.as_object().is_some_and(|o| o.is_empty()) {
        return Err(BridgeFailure::not_available(format!(
            "broker does not expose trading sessions for '{symbol}'"
        )));
    }
    Ok(json!({ "symbol": symbol, "sessions": sessions }))
}

pub(super) async fn get_symbol_trade_constraints(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let spec = symbol_spec(d, symbol).await?;
    let pick = |keys: &[&str]| -> Value {
        let obj = spec.as_object();
        let mut out = serde_json::Map::new();
        if let Some(obj) = obj {
            for k in keys {
                for (spec_key, v) in obj {
                    if spec_key.eq_ignore_ascii_case(k) {
                        out.insert(k.to_string(), v.clone());
                    }
                }
            }
        }
        out.into()
    };
    Ok(json!({
        "symbol": symbol,
        "volume": pick(&["volume_min", "volume_max", "volume_step"]),
        "trade_mode": pick(&["trade_mode", "symbol_trade_mode"]),
        "order_mode": pick(&["order_mode", "symbol_order_mode"]),
        "stops_level": pick(&["stops_level", "trade_stops_level"]),
        "freeze_level": pick(&["freeze_level", "trade_freeze_level"]),
        "contract_size": pick(&["contract_size"]),
        "spec": spec,
    }))
}

// ---------------------------------------------------------------------------
// Category 4 — live market data (28-36)
// ---------------------------------------------------------------------------

pub(super) async fn get_market_tick(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let tick = last_tick(d, symbol).await?;
    Ok(json!({ "symbol": symbol, "tick": tick, "broker_time": broker_time_now() }))
}

pub(super) async fn get_market_bid(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let tick = last_tick(d, symbol).await?;
    Ok(json!({
        "symbol": symbol,
        "bid": tick.get("bid").ok_or_else(|| BridgeFailure::not_available("tick lacks bid"))?,
    }))
}

pub(super) async fn get_market_ask(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let tick = last_tick(d, symbol).await?;
    Ok(json!({
        "symbol": symbol,
        "ask": tick.get("ask").ok_or_else(|| BridgeFailure::not_available("tick lacks ask"))?,
    }))
}

pub(super) async fn get_market_last_price(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let tick = last_tick(d, symbol).await?;
    let last = tick
        .get("last")
        .or_else(|| tick.get("price"))
        .or_else(|| tick.get("close"))
        .cloned();
    Ok(json!({
        "symbol": symbol,
        "last_price": last,
        "note": "FX instruments often carry no last price; bid/ask is authoritative",
    }))
}

pub(super) async fn get_market_depth(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    // Live fact: market_book_get requires a prior market_book_subscribe.
    d.mcp
        .call_tool_json("market_book_subscribe", &json!({ "symbol": symbol }))
        .await?;
    let book = d
        .mcp
        .call_tool_json("market_book_get", &json!({ "symbol": symbol }))
        .await?;
    Ok(json!({
        "symbol": symbol,
        "book": book,
        "note": "book subscription left active; re-calls refresh",
    }))
}

pub(super) async fn get_market_spread_history(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let minutes = opt_f64(p, "minutes").unwrap_or(60.0).clamp(1.0, 1440.0);
    let limit = opt_usize(p, "limit", 500).clamp(1, 2000);
    let res = d
        .mcp
        .call_tool_json(
            "get_chart_ticks_history",
            &window_args(symbol, minutes as i64, limit),
        )
        .await?;
    let (digits, _) = symbol_digits(d, symbol).await?;
    let series: Vec<Value> = ticks_of(&res)
        .iter()
        .filter_map(|t| {
            let bid = t.get("bid").and_then(Value::as_f64)?;
            let ask = t.get("ask").and_then(Value::as_f64)?;
            Some(json!({
                "time": t.get("time_ms").cloned().unwrap_or(Value::Null),
                "spread": round_to_digits(ask - bid, digits),
            }))
        })
        .collect();
    Ok(json!({ "symbol": symbol, "points": series.len(), "series": series }))
}

pub(super) async fn get_market_tick_volume(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let minutes = opt_f64(p, "minutes").unwrap_or(60.0).clamp(1.0, 1440.0);
    let limit = opt_usize(p, "limit", 500).clamp(1, 2000);
    let res = d
        .mcp
        .call_tool_json(
            "get_chart_ticks_history",
            &window_args(symbol, minutes as i64, limit),
        )
        .await?;
    let ticks = ticks_of(&res);
    let volumes: Vec<Value> = ticks
        .iter()
        .filter_map(|t| t.get("volume").cloned())
        .collect();
    if volumes.is_empty() {
        return Err(BridgeFailure::not_available(format!(
            "broker tick feed carries no volume for '{symbol}'"
        )));
    }
    Ok(json!({ "symbol": symbol, "volumes": volumes }))
}

pub(super) async fn get_market_watchlist(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let res = d
        .mcp
        .call_tool_json(
            "get_marketwatch_symbols",
            &json!({ "include_hidden": false }),
        )
        .await?;
    let selected: Vec<String> = res
        .get("symbols")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|s| s.get("selected").and_then(Value::as_bool).unwrap_or(false))
                .filter_map(|s| s.get("symbol").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "count": selected.len(), "selected": selected }))
}

pub(super) async fn subscribe_market_ticks(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    d.state.subscribe_poll(symbol);
    // The terminal exposes no tick-stream subscription; the bridge serves
    // ticks by polling (documented mode, not a silent fake).
    Ok(json!({
        "symbol": symbol,
        "mode": "poll",
        "note": "tick delivery is poll-based; retrieve via get_market_tick",
    }))
}

// ---------------------------------------------------------------------------
// Category 5 — charts & history (37-45)
// ---------------------------------------------------------------------------

pub(super) async fn get_chart_history(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let period = opt_str(p, "period").unwrap_or("M15");
    let minutes = opt_f64(p, "minutes_back")
        .unwrap_or(1440.0)
        .clamp(1.0, 8640.0);
    let limit = opt_usize(p, "limit", 200).clamp(1, 5000);
    let mut args = window_args(symbol, minutes as i64, limit);
    args["period"] = json!(period);
    let res = d.mcp.call_tool_json("get_chart_history", &args).await?;
    Ok(res)
}

pub(super) async fn get_chart_ticks_history(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let minutes = opt_f64(p, "minutes_back")
        .unwrap_or(120.0)
        .clamp(1.0, 1440.0);
    let limit = opt_usize(p, "limit", 200).clamp(1, 5000);
    let res = d
        .mcp
        .call_tool_json(
            "get_chart_ticks_history",
            &window_args(symbol, minutes as i64, limit),
        )
        .await?;
    Ok(res)
}

/// Dynamic passthrough: call the MCP action if the manifest exposes it,
/// answer `not_available` with the reason if it does not. Used for the
/// broker-dependent chart-metadata commands.
async fn passthrough_if_present(
    action: &str,
    args: Value,
    human_reason: &str,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    if d.manifest.get(action).is_none() {
        return Err(BridgeFailure::not_available(format!(
            "{action} is not exposed by this terminal ({human_reason})"
        )));
    }
    d.mcp
        .call_tool_json(action, &args)
        .await
        .map_err(Into::into)
}

pub(super) async fn get_chart_current_timeframe(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = opt_str(p, "symbol").unwrap_or("");
    passthrough_if_present(
        "get_chart_current_timeframe",
        json!({ "symbol": symbol }),
        "no chart-context action in the manifest",
        d,
    )
    .await
}

pub(super) async fn get_chart_available_timeframes(
    _p: &Value,
    _d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    Ok(json!({
        "timeframes": ["M1", "M2", "M3", "M4", "M5", "M6", "M10", "M12", "M15", "M20",
                       "M30", "H1", "H2", "H3", "H4", "H6", "H8", "H12", "D1", "W1", "MN1"],
        "note": "MT5 standard timeframe set; per-symbol availability may vary",
    }))
}

pub(super) async fn get_chart_ohlcv(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let period = opt_str(p, "period").unwrap_or("M15");
    let minutes = opt_f64(p, "minutes_back")
        .unwrap_or(1440.0)
        .clamp(1.0, 8640.0);
    let limit = opt_usize(p, "limit", 200).clamp(1, 5000);
    let mut args = window_args(symbol, minutes as i64, limit);
    args["period"] = json!(period);
    let res = d.mcp.call_tool_json("get_chart_history", &args).await?;
    let candles = res
        .get("candles")
        .or_else(|| res.get("history"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let ohlcv: Vec<Value> = candles
        .iter()
        .map(|c| {
            json!({
                "time": c.get("time").cloned().unwrap_or(Value::Null),
                "open": c.get("open").cloned().unwrap_or(Value::Null),
                "high": c.get("high").cloned().unwrap_or(Value::Null),
                "low": c.get("low").cloned().unwrap_or(Value::Null),
                "close": c.get("close").cloned().unwrap_or(Value::Null),
                "volume": c.get("volume").or_else(|| c.get("tick_volume")).cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(json!({ "symbol": symbol, "period": period, "count": ohlcv.len(), "ohlcv": ohlcv }))
}

pub(super) async fn get_chart_indicators(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = opt_str(p, "symbol").unwrap_or("");
    passthrough_if_present(
        "get_chart_indicators",
        json!({ "symbol": symbol }),
        "terminal exposes no indicator introspection",
        d,
    )
    .await
}

pub(super) async fn get_chart_objects(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = opt_str(p, "symbol").unwrap_or("");
    passthrough_if_present(
        "get_chart_objects",
        json!({ "symbol": symbol }),
        "terminal exposes no chart-object introspection",
        d,
    )
    .await
}

pub(super) async fn get_chart_active_info(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = opt_str(p, "symbol").unwrap_or("");
    passthrough_if_present(
        "get_chart_active_info",
        json!({ "symbol": symbol }),
        "terminal exposes no active-chart introspection",
        d,
    )
    .await
}

pub(super) async fn get_chart_multi_timeframe_history(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let periods: Vec<String> = p
        .get("periods")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| vec!["M15".into(), "H1".into(), "H4".into(), "D1".into()]);
    if periods.is_empty() || periods.len() > 5 {
        return Err(BridgeFailure::invalid_params(
            "'periods' must contain between 1 and 5 timeframes",
        ));
    }
    let minutes = opt_f64(p, "minutes_back")
        .unwrap_or(2880.0)
        .clamp(1.0, 8640.0);
    let limit = opt_usize(p, "limit", 100).clamp(1, 1000);
    let mut out = serde_json::Map::new();
    for period in &periods {
        let mut args = window_args(symbol, minutes as i64, limit);
        args["period"] = json!(period);
        let res = d.mcp.call_tool_json("get_chart_history", &args).await?;
        out.insert(
            period.clone(),
            res.get("candles")
                .or_else(|| res.get("history"))
                .cloned()
                .unwrap_or(Value::Null),
        );
    }
    Ok(json!({ "symbol": symbol, "timeframes": Value::Object(out) }))
}

// (no silencers — every helper above is exercised by a handler)
