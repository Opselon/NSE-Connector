//! Bridge handlers — categories 6-8: positions, orders & history, trade
//! execution. Financial commands map 1:1 onto the MT5 MCP trade actions and
//! are gated Tier 3; composite commands (close-all, reverse) execute the
//! underlying actions sequentially and report per-step results.

use serde_json::{Value, json};

use super::{BridgeDeps, BridgeFailure, opt_f64, opt_str, req_f64, req_str};
use crate::mcp::trading_tools;

const MAX_POSITIONS_PER_COMPOSITE: usize = 50;
/// Composite orders never exceed this volume (platform-side limits remain
/// authoritative; this is the bridge-local hard stop).
const MAX_VOLUME_PER_ORDER: f64 = 100.0;

/// Broker ids may arrive as strings or numbers (live fact: string, but
/// numeric toleration costs nothing).
fn id_of(item: &Value, key: &str) -> Option<String> {
    match item.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared fetchers
// ---------------------------------------------------------------------------

async fn open_positions(
    d: &mut BridgeDeps<'_>,
    include_orders: bool,
) -> Result<(Vec<Value>, Vec<Value>), BridgeFailure> {
    let res = d
        .mcp
        .call_tool_json(
            trading_tools::OPEN_POSITIONS,
            &json!({ "include_orders": include_orders }),
        )
        .await?;
    let positions = res
        .get("positions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let orders = res
        .get("orders")
        .or_else(|| res.get("pending_orders"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok((positions, orders))
}

fn by_ticket<'a>(items: &'a [Value], ticket: &str, id_key: &str) -> Option<&'a Value> {
    items
        .iter()
        .find(|it| id_of(it, id_key).as_deref() == Some(ticket))
}

/// Volume guard applied before any financial composite.
fn validate_volume(volume: f64) -> Result<(), BridgeFailure> {
    if volume <= 0.0 || !volume.is_finite() {
        return Err(BridgeFailure::invalid_params("volume must be > 0"));
    }
    if volume > MAX_VOLUME_PER_ORDER {
        return Err(BridgeFailure::invalid_params(format!(
            "volume {volume} exceeds the bridge hard cap of {MAX_VOLUME_PER_ORDER}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Category 6 — positions (46-54)
// ---------------------------------------------------------------------------

pub(super) async fn get_trading_open_positions(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let include_orders = p
        .get("include_orders")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let (positions, orders) = open_positions(d, include_orders).await?;
    Ok(json!({
        "positions_count": positions.len(),
        "orders_count": orders.len(),
        "positions": positions,
        "orders": orders,
    }))
}

async fn find_position(d: &mut BridgeDeps<'_>, ticket: &str) -> Result<Value, BridgeFailure> {
    let (positions, _) = open_positions(d, false).await?;
    by_ticket(&positions, ticket, "position_id")
        .cloned()
        .ok_or_else(|| {
            BridgeFailure::not_available(format!(
                "position '{ticket}' not found among open positions"
            ))
        })
}

pub(super) async fn get_position_info(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ticket = req_str(p, "position_ticket")?;
    let pos = find_position(d, ticket).await?;
    Ok(json!({ "position": pos }))
}

pub(super) async fn get_position_profit(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ticket = req_str(p, "position_ticket")?;
    let pos = find_position(d, ticket).await?;
    Ok(json!({
        "position_ticket": ticket,
        "profit": pos.get("profit"),
        "swap": pos.get("swap"),
        "commission": pos.get("commission"),
    }))
}

pub(super) async fn get_position_volume(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ticket = req_str(p, "position_ticket")?;
    let pos = find_position(d, ticket).await?;
    Ok(json!({
        "position_ticket": ticket,
        "volume": pos.get("volume"),
        "contract_size": pos.get("contract_size"),
    }))
}

pub(super) async fn get_position_sl_tp(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ticket = req_str(p, "position_ticket")?;
    let pos = find_position(d, ticket).await?;
    Ok(json!({
        "position_ticket": ticket,
        "sl": pos.get("sl"),
        "tp": pos.get("tp"),
    }))
}

pub(super) async fn get_position_risk(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ticket = req_str(p, "position_ticket")?;
    let pos = find_position(d, ticket).await?;
    let symbol = pos.get("symbol").and_then(Value::as_str).unwrap_or("");
    let volume = pos.get("volume").and_then(Value::as_f64).unwrap_or(0.0);
    let action = pos.get("action").and_then(Value::as_str).unwrap_or("buy");
    let open_price = pos.get("price_open").and_then(Value::as_f64);
    let sl = pos.get("sl").and_then(Value::as_f64);
    let tp = pos.get("tp").and_then(Value::as_f64);

    let mut out = json!({
        "position_ticket": ticket,
        "symbol": symbol,
        "volume": volume,
        "sl": sl,
        "tp": tp,
        "current_profit": pos.get("profit"),
    });
    // Margin actually tied up by this position (0.1-lot-scale calc tools
    // accept arbitrary volume — verified live).
    if !symbol.is_empty()
        && volume > 0.0
        && let Ok(margin) = d
            .mcp
            .call_tool_json(
                "trade_order_calc_margin",
                &json!({ "symbol": symbol, "type": action, "volume": volume, "price": open_price }),
            )
            .await
    {
        out["margin_required"] = margin.get("margin").cloned().unwrap_or(Value::Null);
    }
    // Hypothetical outcome exactly at SL / TP.
    if let (Some(sl), Some(open_price)) = (sl, open_price)
        && let Ok(profit_sl) = d
            .mcp
            .call_tool_json(
                "trade_order_calc_profit",
                &json!({
                    "symbol": symbol, "type": action, "volume": volume,
                    "price_open": open_price, "price_close": sl,
                }),
            )
            .await
    {
        out["loss_at_sl"] = profit_sl.get("profit").cloned().unwrap_or(Value::Null);
    }
    if let (Some(tp), Some(open_price)) = (tp, open_price)
        && let Ok(profit_tp) = d
            .mcp
            .call_tool_json(
                "trade_order_calc_profit",
                &json!({
                    "symbol": symbol, "type": action, "volume": volume,
                    "price_open": open_price, "price_close": tp,
                }),
            )
            .await
    {
        out["profit_at_tp"] = profit_tp.get("profit").cloned().unwrap_or(Value::Null);
    }
    Ok(out)
}

pub(super) async fn trade_modify_sl_tp(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    if opt_str(p, "position_ticket").is_none() && opt_str(p, "order_ticket").is_none() {
        return Err(BridgeFailure::invalid_params(
            "either 'position_ticket' or 'order_ticket' is required",
        ));
    }
    let args = json!({
        "symbol": symbol,
        "position_ticket": opt_str(p, "position_ticket"),
        "order_ticket": opt_str(p, "order_ticket"),
        "sl": opt_f64(p, "sl"),
        "tp": opt_f64(p, "tp"),
    });
    let res = d.mcp.call_tool_json("trade_modify_sl_tp", &args).await?;
    Ok(json!({ "modified": true, "result": res }))
}

pub(super) async fn trade_close_single_position(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let ticket = req_str(p, "position_ticket")?;
    let res = d
        .mcp
        .call_tool_json(
            "trade_close_single_position",
            &json!({ "symbol": symbol, "position_ticket": ticket }),
        )
        .await?;
    Ok(json!({ "closed": ticket, "result": res }))
}

pub(super) async fn trade_close_all_positions(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = opt_str(p, "symbol");
    let (positions, _) = open_positions(d, false).await?;
    if positions.len() > MAX_POSITIONS_PER_COMPOSITE {
        return Err(BridgeFailure::invalid_params(format!(
            "{} open positions exceed the composite cap of {MAX_POSITIONS_PER_COMPOSITE}",
            positions.len()
        )));
    }
    let mut results = Vec::with_capacity(positions.len());
    for pos in &positions {
        let pos_symbol = pos.get("symbol").and_then(Value::as_str).unwrap_or("");
        if let Some(filter) = symbol
            && pos_symbol != filter
        {
            continue;
        }
        let ticket = id_of(pos, "position_id").unwrap_or_default();
        let res = d
            .mcp
            .call_tool_json(
                "trade_close_single_position",
                &json!({ "symbol": pos_symbol, "position_ticket": ticket }),
            )
            .await;
        results.push(json!({
            "position_ticket": ticket,
            "symbol": pos_symbol,
            "outcome": match res {
                Ok(v) => json!({ "ok": true, "result": v }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            },
        }));
    }
    let closed = results
        .iter()
        .filter(|r| r["outcome"]["ok"] == json!(true))
        .count();
    Ok(json!({
        "attempted": results.len(),
        "closed": closed,
        "results": results,
    }))
}

// ---------------------------------------------------------------------------
// Category 7 — orders & history (55-63)
// ---------------------------------------------------------------------------

pub(super) async fn get_trading_pending_orders(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let (_, orders) = open_positions(d, true).await?;
    Ok(json!({ "count": orders.len(), "orders": orders }))
}

pub(super) async fn get_order_info(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ticket = req_str(p, "order_ticket")?;
    // Working orders first, then the recent history window.
    let (_, working) = open_positions(d, true).await?;
    if let Some(o) = by_ticket(&working, ticket, "order_id") {
        return Ok(json!({ "order": o, "where": "working" }));
    }
    let args = super::query::window_args("EURUSD", 60 * 24 * 30, 1000);
    let res = d
        .mcp
        .call_tool_json(trading_tools::HISTORY_ORDERS, &args)
        .await?;
    let hist = res
        .get("orders")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    by_ticket(&hist, ticket, "order_id")
        .map(|o| json!({ "order": o, "where": "history" }))
        .ok_or_else(|| {
            BridgeFailure::not_available(format!(
                "order '{ticket}' not found in working or recent history"
            ))
        })
}

pub(super) async fn get_order_status(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let info = get_order_info(p, d).await?;
    let order = info.get("order").cloned().unwrap_or(json!({}));
    Ok(json!({
        "order_ticket": order.get("order_id"),
        "state": order.get("state"),
        "where": info.get("where"),
        "volume_initial": order.get("volume_initial"),
        "volume_current": order.get("volume_current"),
    }))
}

pub(super) async fn get_trading_history_orders(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let args = history_args(p);
    let res = d
        .mcp
        .call_tool_json(trading_tools::HISTORY_ORDERS, &args)
        .await?;
    Ok(res)
}

pub(super) async fn get_trading_history_positions(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let args = history_args(p);
    let res = d
        .mcp
        .call_tool_json(trading_tools::HISTORY_POSITIONS, &args)
        .await?;
    Ok(res)
}

pub(super) async fn get_trading_deals_history(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let mut args = history_args(p);
    args["include_deals"] = json!(true);
    let res = d
        .mcp
        .call_tool_json(trading_tools::HISTORY_ORDERS, &args)
        .await?;
    let deals = res.get("deals").cloned().unwrap_or_else(|| json!([]));
    Ok(json!({ "deals": deals }))
}

fn history_args(p: &Value) -> Value {
    let days = opt_f64(p, "days_back").unwrap_or(7.0).clamp(0.01, 3650.0);
    let now = super::broker_time_now();
    let from = (time::OffsetDateTime::now_utc() - time::Duration::seconds((days * 86400.0) as i64))
        .format(&time::format_description::well_known::Rfc3339)
        .map(|s| s[..s.len().min(19)].to_string())
        .unwrap_or_else(|_| "1970-01-01T00:00:00".into());
    json!({
        "datetime_from": from,
        "datetime_to": now,
        "include_orders": true,
        "include_orders_canceled": true,
        "include_deals": true,
        "limit": p.get("limit").and_then(Value::as_u64).unwrap_or(500).clamp(1, 5000),
    })
}

pub(super) async fn trade_cancel_pending_order(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let ticket = req_str(p, "order_ticket")?;
    let res = d
        .mcp
        .call_tool_json(
            "trade_delete_order",
            &json!({ "symbol": symbol, "order_ticket": ticket }),
        )
        .await?;
    Ok(json!({ "cancelled": ticket, "result": res }))
}

pub(super) async fn trade_modify_pending_order(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let ticket = req_str(p, "order_ticket")?;
    // Live schema fact: trade_modify_sl_tp carries no price/volume/expiration
    // parameter — pending PRICE modification is structurally impossible via
    // MCP; SL/TP modification is supported (verified in cycle 1).
    if opt_f64(p, "price").is_some() {
        return Err(BridgeFailure::not_available(
            "pending order price/volume/expiration modify is not exposed by the MT5 MCP actions; SL/TP only",
        ));
    }
    let args = json!({
        "symbol": symbol,
        "order_ticket": ticket,
        "sl": opt_f64(p, "sl"),
        "tp": opt_f64(p, "tp"),
    });
    let res = d.mcp.call_tool_json("trade_modify_sl_tp", &args).await?;
    Ok(json!({ "modified": ticket, "result": res }))
}

pub(super) async fn get_order_execution_details(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ticket = req_str(p, "order_ticket")?;
    let args = history_args(p);
    let res = d
        .mcp
        .call_tool_json(trading_tools::HISTORY_ORDERS, &args)
        .await?;
    let deals = res
        .get("deals")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mine: Vec<Value> = deals
        .into_iter()
        .filter(|deal| id_of(deal, "order_id").as_deref() == Some(ticket))
        .collect();
    if mine.is_empty() {
        return Err(BridgeFailure::not_available(format!(
            "no execution deals found for order '{ticket}' in the requested window"
        )));
    }
    Ok(json!({ "order_ticket": ticket, "deals": mine }))
}

// ---------------------------------------------------------------------------
// Category 8 — trade execution (64-72)
// ---------------------------------------------------------------------------

pub(super) async fn trade_send_market_order(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let order_type = req_str(p, "type")?;
    let volume = req_f64(p, "volume")?;
    validate_volume(volume)?;
    let args = json!({
        "symbol": symbol,
        "type": order_type,
        "volume": volume,
        "sl": opt_f64(p, "sl"),
        "tp": opt_f64(p, "tp"),
        "comment": opt_str(p, "comment"),
    });
    let res = d
        .mcp
        .call_tool_json("trade_send_market_order", &args)
        .await?;
    Ok(json!({ "sent": true, "result": res }))
}

pub(super) async fn trade_send_buy_order(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let mut params = p.clone();
    params["type"] = json!("buy");
    trade_send_market_order(&params, d).await
}

pub(super) async fn trade_send_sell_order(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let mut params = p.clone();
    params["type"] = json!("sell");
    trade_send_market_order(&params, d).await
}

pub(super) async fn trade_send_pending_order(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let order_type = req_str(p, "type")?;
    let volume = req_f64(p, "volume")?;
    let price = req_f64(p, "price")?;
    validate_volume(volume)?;
    let args = json!({
        "symbol": symbol,
        "type": order_type,
        "volume": volume,
        "price": price,
        "stoplimit": opt_f64(p, "stoplimit"),
        "sl": opt_f64(p, "sl"),
        "tp": opt_f64(p, "tp"),
        "filling_type": opt_str(p, "filling_type"),
        "expiration_type": opt_str(p, "expiration_type"),
        "expiration_time": opt_str(p, "expiration_time"),
        "comment": opt_str(p, "comment"),
    });
    let res = d
        .mcp
        .call_tool_json("trade_send_pending_order", &args)
        .await?;
    Ok(json!({ "placed": true, "result": res }))
}

pub(super) async fn trade_close_partial_position(
    _p: &Value,
    _d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    // Schema-verified (cycle 1): trade_close_single_position has NO volume
    // parameter, so partial close is structurally impossible via the current
    // MCP actions. Never fake it with an opposite market order — on hedging
    // accounts that opens a NEW position instead of reducing this one.
    Err(BridgeFailure::not_available(
        "partial close is not exposed by the MT5 MCP schema (trade_close_single_position has no volume parameter); \
         close the full position or place an opposite order knowing it opens a separate hedged position",
    ))
}

pub(super) async fn trade_reverse_position(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let ticket = req_str(p, "position_ticket")?;
    let pos = find_position(d, ticket).await?;
    let symbol = pos
        .get("symbol")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let action = pos
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("buy")
        .to_lowercase();
    let volume = pos.get("volume").and_then(Value::as_f64).unwrap_or(0.0);
    if symbol.is_empty() || volume <= 0.0 {
        return Err(BridgeFailure::invalid_params(
            "position record lacks symbol/volume — cannot reverse",
        ));
    }
    validate_volume(volume)?;
    let opposite = if action == "buy" { "sell" } else { "buy" };
    // Step 1: close the existing position.
    let close_res = d
        .mcp
        .call_tool_json(
            "trade_close_single_position",
            &json!({ "symbol": symbol, "position_ticket": ticket }),
        )
        .await?;
    // Step 2: open the opposite side with the same volume.
    let open_res = d
        .mcp
        .call_tool_json(
            "trade_send_market_order",
            &json!({
                "symbol": symbol,
                "type": opposite,
                "volume": volume,
                "comment": opt_str(p, "comment").map(|c| format!("reverse of {ticket}: {c}")).unwrap_or_else(|| format!("reverse of {ticket}")),
            }),
        )
        .await?;
    Ok(json!({
        "reversed": ticket,
        "closed_side": action,
        "opened_side": opposite,
        "volume": volume,
        "close_result": close_res,
        "open_result": open_res,
        "note": "if the close failed server-side the open still executes — verify positions afterwards",
    }))
}

pub(super) async fn trade_set_position_sl(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let ticket = req_str(p, "position_ticket")?;
    let sl = req_f64(p, "sl")?;
    let res = d
        .mcp
        .call_tool_json(
            "trade_modify_sl_tp",
            &json!({ "symbol": symbol, "position_ticket": ticket, "sl": sl }),
        )
        .await?;
    Ok(json!({ "position_ticket": ticket, "sl": sl, "result": res }))
}

pub(super) async fn trade_set_position_tp(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let ticket = req_str(p, "position_ticket")?;
    let tp = req_f64(p, "tp")?;
    let res = d
        .mcp
        .call_tool_json(
            "trade_modify_sl_tp",
            &json!({ "symbol": symbol, "position_ticket": ticket, "tp": tp }),
        )
        .await?;
    Ok(json!({ "position_ticket": ticket, "tp": tp, "result": res }))
}

pub(super) async fn trade_calculate_order_result(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let order_type = opt_str(p, "type").unwrap_or("buy").to_string();
    let volume = req_f64(p, "volume")?;
    let entry = req_f64(p, "entry_price")?;
    let sl = opt_f64(p, "sl_price");
    let tp = opt_f64(p, "tp_price");
    let mut out = json!({
        "symbol": symbol,
        "type": order_type,
        "volume": volume,
        "entry_price": entry,
    });
    if let Ok(margin) = d
        .mcp
        .call_tool_json(
            "trade_order_calc_margin",
            &json!({ "symbol": symbol, "type": order_type, "volume": volume, "price": entry }),
        )
        .await
    {
        out["margin_required"] = margin.get("margin").cloned().unwrap_or(Value::Null);
    }
    if let Some(tp) = tp
        && let Ok(profit) = d
            .mcp
            .call_tool_json(
                "trade_order_calc_profit",
                &json!({
                    "symbol": symbol, "type": order_type, "volume": volume,
                    "price_open": entry, "price_close": tp,
                }),
            )
            .await
    {
        out["profit_at_tp"] = profit.get("profit").cloned().unwrap_or(Value::Null);
    }
    if let Some(sl) = sl
        && let Ok(profit) = d
            .mcp
            .call_tool_json(
                "trade_order_calc_profit",
                &json!({
                    "symbol": symbol, "type": order_type, "volume": volume,
                    "price_open": entry, "price_close": sl,
                }),
            )
            .await
    {
        out["loss_at_sl"] = profit.get("profit").cloned().unwrap_or(Value::Null);
    }
    Ok(out)
}
