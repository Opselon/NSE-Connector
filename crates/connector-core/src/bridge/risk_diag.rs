//! Bridge handlers — categories 9-10: risk & calculations, errors &
//! monitoring. Calculators derive from live broker data (spec digits, calc
//! tools) with every intermediate value exposed, so the Platform can audit
//! the arithmetic instead of trusting a black box.

use serde_json::{Value, json};

use super::{
    BRIDGE_COMMANDS, BridgeDeps, BridgeFailure, opt_f64, opt_str, opt_usize, req_f64, req_str,
    round_to_digits,
};
use crate::bridge::query::{account_info, last_tick, symbol_digits, symbol_spec};

// ---------------------------------------------------------------------------
// Category 9 — risk & calculations (73-81)
// ---------------------------------------------------------------------------

/// Per-pip, per-lot value in account currency, measured with the broker's
/// own profit calculator: profit(1 lot, 10 pips) / 10. The EURUSD live
/// discovery showed tick_value/tick_size can be 0.0 — measuring through the
/// calc tool is the honest derivation.
async fn pip_value_per_lot(
    d: &mut BridgeDeps<'_>,
    symbol: &str,
    digits: u32,
) -> Result<(f64, bool), BridgeFailure> {
    // Spec tick_value path first (broker-authoritative when non-zero).
    let spec = symbol_spec(d, symbol).await?;
    let tick_value = spec
        .get("tick_value")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let tick_size = spec.get("tick_size").and_then(Value::as_f64).unwrap_or(0.0);
    if tick_value > 0.0 && tick_size > 0.0 {
        // pip = 10 * tick_size is the standard FX pip definition.
        let pip_size = tick_size * 10.0;
        let pips = pip_size / tick_size;
        return Ok((tick_value * pips, false));
    }
    // Measured path: 10-pip profit on 1 lot via the broker's own calculator
    // (live fact: tick_value/tick_size can be 0.0 on MetaQuotes-Demo).
    let tick = last_tick(d, symbol).await?;
    let Some(bid) = tick.get("bid").and_then(Value::as_f64) else {
        return Err(BridgeFailure::not_available(format!(
            "cannot measure pip value for '{symbol}': no live tick and no tick_value in spec"
        )));
    };
    let ten_pips = 10.0 * 10f64.powi(-(digits as i32 - 1));
    let res = d
        .mcp
        .call_tool_json(
            "trade_order_calc_profit",
            &json!({
                "symbol": symbol, "type": "buy", "volume": 1.0,
                "price_open": bid,
                "price_close": round_to_digits(bid + ten_pips, digits),
            }),
        )
        .await?;
    let profit = res.get("profit").and_then(Value::as_f64).ok_or_else(|| {
        BridgeFailure::not_available("profit calculator returned no profit field")
    })?;
    Ok((profit / 10.0, true))
}

pub(super) async fn calculate_position_size(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let risk_amount = if let Some(a) = opt_f64(p, "risk_amount") {
        a
    } else if let Some(pct) = opt_f64(p, "risk_percent") {
        let equity = match opt_f64(p, "equity") {
            Some(e) => e,
            None => account_info(d)
                .await?
                .get("equity")
                .and_then(Value::as_f64)
                .ok_or_else(|| {
                    BridgeFailure::invalid_params("no equity available — pass 'equity' explicitly")
                })?,
        };
        equity * pct / 100.0
    } else {
        return Err(BridgeFailure::invalid_params(
            "either 'risk_amount' or 'risk_percent' (+ optional 'equity') is required",
        ));
    };
    if risk_amount <= 0.0 {
        return Err(BridgeFailure::invalid_params("risk budget must be > 0"));
    }
    let (digits, spec) = symbol_digits(d, symbol).await?;
    // SL distance: explicit pips, or entry/SL prices.
    let sl_pips = if let Some(pips) = opt_f64(p, "sl_pips") {
        pips
    } else if let (Some(entry), Some(sl)) = (opt_f64(p, "entry_price"), opt_f64(p, "sl_price")) {
        ((entry - sl).abs() * 10f64.powi(digits as i32)) / 10.0
    } else {
        return Err(BridgeFailure::invalid_params(
            "either 'sl_pips' or 'entry_price'+'sl_price' is required",
        ));
    };
    if sl_pips <= 0.0 {
        return Err(BridgeFailure::invalid_params("SL distance must be > 0"));
    }
    let (pip_value, measured) = pip_value_per_lot(d, symbol, digits).await?;
    let raw_volume = risk_amount / (sl_pips * pip_value);
    // Normalize down to the broker's volume step.
    let step = spec
        .get("volume_step")
        .and_then(Value::as_f64)
        .unwrap_or(0.01);
    let min = spec
        .get("volume_min")
        .and_then(Value::as_f64)
        .unwrap_or(0.01);
    let max = spec
        .get("volume_max")
        .and_then(Value::as_f64)
        .unwrap_or(100.0);
    let stepped = if step > 0.0 {
        (raw_volume / step).floor() * step
    } else {
        raw_volume
    };
    let volume = stepped.clamp(0.0, max);
    Ok(json!({
        "symbol": symbol,
        "risk_amount": risk_amount,
        "sl_pips": sl_pips,
        "pip_value_per_lot": pip_value,
        "pip_value_measured_via_calc": measured,
        "raw_volume": raw_volume,
        "volume_step": step,
        "volume_min": min,
        "recommended_volume": volume,
        "fits_volume_min": volume >= min,
        "note": if volume < min {
            "risk budget too small for the broker's minimum volume — do not place this order"
        } else {
            ""
        },
    }))
}

pub(super) async fn calculate_trade_risk(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let order_type = opt_str(p, "type").unwrap_or("buy").to_string();
    let volume = req_f64(p, "volume")?;
    let entry = req_f64(p, "entry_price")?;
    let sl = req_f64(p, "sl_price")?;
    let equity = match opt_f64(p, "equity") {
        Some(e) => e,
        None => account_info(d)
            .await?
            .get("equity")
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                BridgeFailure::invalid_params("no equity available — pass 'equity' explicitly")
            })?,
    };
    let margin = d
        .mcp
        .call_tool_json(
            "trade_order_calc_margin",
            &json!({ "symbol": symbol, "type": order_type, "volume": volume, "price": entry }),
        )
        .await?
        .get("margin")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let loss = d
        .mcp
        .call_tool_json(
            "trade_order_calc_profit",
            &json!({
                "symbol": symbol, "type": order_type, "volume": volume,
                "price_open": entry, "price_close": sl,
            }),
        )
        .await?
        .get("profit")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    Ok(json!({
        "symbol": symbol,
        "volume": volume,
        "margin_required": margin,
        "loss_at_sl": loss,
        "equity": equity,
        "risk_percent_of_equity": if equity > 0.0 { loss.abs() / equity * 100.0 } else { 0.0 },
    }))
}

fn sl_tp_price(
    side: &str,
    entry: f64,
    pips: f64,
    digits: u32,
    is_sl: bool,
) -> Result<f64, BridgeFailure> {
    let pip = 10f64.powi(-(digits as i32 - 1));
    let buy = side.eq_ignore_ascii_case("buy");
    // BUY: SL below entry, TP above. SELL: mirrored.
    let signed = if (buy && is_sl) || (!buy && !is_sl) {
        -pips
    } else {
        pips
    };
    if pips <= 0.0 {
        return Err(BridgeFailure::invalid_params("pips must be > 0"));
    }
    Ok(round_to_digits(entry + signed * pip, digits))
}

pub(super) async fn calculate_stop_loss_price(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let side = opt_str(p, "type").unwrap_or("buy").to_string();
    let entry = req_f64(p, "entry_price")?;
    let pips = req_f64(p, "sl_pips")?;
    let (digits, _) = symbol_digits(d, symbol).await?;
    let price = sl_tp_price(&side, entry, pips, digits, true)?;
    Ok(json!({
        "symbol": symbol,
        "type": side,
        "entry_price": entry,
        "sl_pips": pips,
        "sl_price": price,
        "digits": digits,
    }))
}

pub(super) async fn calculate_take_profit_price(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let side = opt_str(p, "type").unwrap_or("buy").to_string();
    let entry = req_f64(p, "entry_price")?;
    let pips = req_f64(p, "tp_pips")?;
    let (digits, _) = symbol_digits(d, symbol).await?;
    let price = sl_tp_price(&side, entry, pips, digits, false)?;
    Ok(json!({
        "symbol": symbol,
        "type": side,
        "entry_price": entry,
        "tp_pips": pips,
        "tp_price": price,
        "digits": digits,
    }))
}

pub(super) async fn calculate_margin_required(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let order_type = opt_str(p, "type").unwrap_or("buy").to_string();
    let volume = req_f64(p, "volume")?;
    let price = opt_f64(p, "price");
    let res = d
        .mcp
        .call_tool_json(
            "trade_order_calc_margin",
            &json!({ "symbol": symbol, "type": order_type, "volume": volume, "price": price }),
        )
        .await?;
    Ok(res)
}

pub(super) async fn calculate_margin_level_after_trade(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let order_type = opt_str(p, "type").unwrap_or("buy").to_string();
    let volume = req_f64(p, "volume")?;
    let acc = account_info(d).await?;
    let equity = acc.get("equity").and_then(Value::as_f64).unwrap_or(0.0);
    let margin_now = acc.get("margin").and_then(Value::as_f64).unwrap_or(0.0);
    let required = d
        .mcp
        .call_tool_json(
            "trade_order_calc_margin",
            &json!({
                "symbol": symbol, "type": order_type, "volume": volume,
                "price": opt_f64(p, "price"),
            }),
        )
        .await?
        .get("margin")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let after = margin_now + required;
    Ok(json!({
        "equity": equity,
        "margin_current": margin_now,
        "margin_new_order": required,
        "margin_total_after": after,
        "projected_margin_level_percent": if after > 0.0 {
            Some(equity / after * 100.0)
        } else {
            None
        },
        "note": "level below 100% means the order cannot be safely opened",
    }))
}

pub(super) async fn calculate_pip_value(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let lots = opt_f64(p, "volume").unwrap_or(1.0);
    let (digits, _) = symbol_digits(d, symbol).await?;
    let (per_lot, measured) = pip_value_per_lot(d, symbol, digits).await?;
    Ok(json!({
        "symbol": symbol,
        "pip_value_per_lot": per_lot,
        "volume": lots,
        "pip_value_total": per_lot * lots,
        "measured_via_profit_calc": measured,
    }))
}

pub(super) async fn calculate_profit_loss(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let order_type = opt_str(p, "type").unwrap_or("buy").to_string();
    let volume = req_f64(p, "volume")?;
    let entry = req_f64(p, "price_open")?;
    let close = req_f64(p, "price_close")?;
    let res = d
        .mcp
        .call_tool_json(
            "trade_order_calc_profit",
            &json!({
                "symbol": symbol, "type": order_type, "volume": volume,
                "price_open": entry, "price_close": close,
            }),
        )
        .await?;
    Ok(res)
}

pub(super) async fn validate_trade_request(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let symbol = req_str(p, "symbol")?;
    let order_type = req_str(p, "type")?;
    let volume = req_f64(p, "volume")?;
    let res = if let Some(price) = opt_f64(p, "price") {
        // Pending-shaped request.
        d.mcp
            .call_tool_json(
                "trade_check_pending_order",
                &json!({
                    "symbol": symbol, "type": order_type, "volume": volume,
                    "price": price,
                    "stoplimit": opt_f64(p, "stoplimit"),
                    "sl": opt_f64(p, "sl"), "tp": opt_f64(p, "tp"),
                }),
            )
            .await?
    } else {
        d.mcp
            .call_tool_json(
                "trade_check_market_order",
                &json!({
                    "symbol": symbol, "type": order_type, "volume": volume,
                    "sl": opt_f64(p, "sl"), "tp": opt_f64(p, "tp"),
                }),
            )
            .await?
    };
    Ok(
        json!({ "kind": if opt_f64(p, "price").is_some() { "pending" } else { "market" }, "validation": res }),
    )
}

// ---------------------------------------------------------------------------
// Category 10 — errors & monitoring (82-90)
// ---------------------------------------------------------------------------

pub(super) async fn get_mt5_last_error(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let state = d.state.inner.read().unwrap_or_else(|e| e.into_inner());
    Ok(json!({
        "last_error": state.last_error,
        "mt5_connected": state.mt5_connected,
    }))
}

pub(super) async fn get_bridge_logs(
    p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let limit = opt_usize(p, "limit", 50).clamp(1, 200);
    let state = d.state.inner.read().unwrap_or_else(|e| e.into_inner());
    let logs: Vec<&String> = state.logs.iter().rev().take(limit).collect();
    Ok(json!({ "count": logs.len(), "logs": logs }))
}

pub(super) async fn get_bridge_health(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let s = d.state.inner.read().unwrap_or_else(|e| e.into_inner());
    let failure_rate = if s.commands_total > 0 {
        (s.mcp_failed as f64 + s.rejected as f64) / s.commands_total as f64 * 100.0
    } else {
        0.0
    };
    let verdict = if s.mt5_connected && failure_rate < 25.0 {
        "healthy"
    } else if s.mt5_connected {
        "degraded"
    } else {
        "disconnected"
    };
    Ok(json!({
        "verdict": verdict,
        "uptime_secs": s.started_at.elapsed().as_secs(),
        "mt5_connected": s.mt5_connected,
        "commands_total": s.commands_total,
        "failure_rate_percent": round_to_digits(failure_rate, 2),
        "last_command": s.last_command,
        "last_error": s.last_error,
    }))
}

pub(super) async fn get_bridge_latency(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let s = d.state.inner.read().unwrap_or_else(|e| e.into_inner());
    let avg = s
        .total_latency_ms
        .checked_div(s.commands_total)
        .unwrap_or(0);
    Ok(json!({
        "avg_command_latency_ms": avg,
        "last_command_latency_ms": s.last_latency_ms,
        "commands_total": s.commands_total,
    }))
}

pub(super) async fn get_mt5_connection_metrics(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let (connected, session_age, reconnects, last_error) = {
        let s = d.state.inner.read().unwrap_or_else(|e| e.into_inner());
        (
            s.mt5_connected,
            s.connected_since.map(|t| t.elapsed().as_secs()),
            s.reconnects,
            s.last_error.clone(),
        )
    };
    let mut out = json!({
        "mt5_connected": connected,
        "session_age_secs": session_age,
        "reconnects": reconnects,
        "last_error": last_error,
    });
    // Guard dropped above — the MCP call may await freely.
    if let Ok(ti) = d.mcp.call_tool_json("terminal_get_info", &json!({})).await {
        if let Some(ping) = ti.pointer("/terminal/connection/ping_microseconds") {
            out["terminal_ping_microseconds"] = ping.clone();
        }
        if let Some(state_str) = ti.pointer("/terminal/connection/state") {
            out["terminal_connection_state"] = state_str.clone();
        }
    }
    Ok(out)
}

pub(super) async fn get_trade_execution_metrics(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let s = d.state.inner.read().unwrap_or_else(|e| e.into_inner());
    let avg = s
        .total_latency_ms
        .checked_div(s.commands_total)
        .unwrap_or(0);
    Ok(json!({
        "executed": s.executed,
        "rejected": s.rejected,
        "mcp_failed": s.mcp_failed,
        "total": s.commands_total,
        "avg_latency_ms": avg,
        "last_command": s.last_command,
    }))
}

pub(super) async fn get_bridge_capabilities(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let commands: Vec<Value> = BRIDGE_COMMANDS
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "category": c.category,
                "tier": c.tier.as_str(),
                "summary": c.summary,
            })
        })
        .collect();
    Ok(json!({
        "bridge_version": env!("CARGO_PKG_VERSION"),
        "command_count": commands.len(),
        "commands": commands,
        "mcp_actions_discovered": d.manifest.action_count,
        "mcp_manifest_hash": format!("{:x}", d.manifest.content_hash),
    }))
}

pub(super) async fn get_mcp_available_tools(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    let tools = d.mcp.list_tools().await?;
    let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    Ok(json!({ "count": names.len(), "tools": names }))
}

pub(super) async fn get_mt5_diagnostic_report(
    _p: &Value,
    d: &mut BridgeDeps<'_>,
) -> Result<Value, BridgeFailure> {
    // Composite: terminal info + connection metrics + capabilities summary.
    // Each section degrades independently — a failed probe is reported, not fatal.
    let mut report = json!({ "generated_at_broker_time": super::broker_time_now() });
    match d.mcp.call_tool_json("terminal_get_info", &json!({})).await {
        Ok(ti) => {
            report["terminal"] = ti.get("terminal").cloned().unwrap_or(ti);
        }
        Err(e) => {
            report["terminal"] = json!({ "probe_failed": e.to_string() });
            d.state
                .set_last_error(format!("diagnostic terminal probe: {e}"));
        }
    }
    let connection = get_mt5_connection_metrics(&json!({}), d).await;
    match connection {
        Ok(v) => report["connection"] = v,
        Err(e) => report["connection"] = json!({ "probe_failed": e.message }),
    }
    match d.mcp.list_tools().await {
        Ok(tools) => report["mcp_tools"] = json!(tools.len()),
        Err(e) => report["mcp_tools"] = json!({ "probe_failed": e.to_string() }),
    }
    let s = d.state.inner.read().unwrap_or_else(|e| e.into_inner());
    report["bridge"] = json!({
        "uptime_secs": s.started_at.elapsed().as_secs(),
        "commands_total": s.commands_total,
        "outcome_counts": {
            "executed": s.executed,
            "rejected": s.rejected,
            "mcp_failed": s.mcp_failed,
        },
        "last_error": s.last_error,
    });
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sl_tp_prices_side_aware_and_digit_correct() {
        // BUY: SL below, TP above.
        assert_eq!(sl_tp_price("buy", 1.10000, 20.0, 5, true).unwrap(), 1.09800);
        assert_eq!(
            sl_tp_price("buy", 1.10000, 40.0, 5, false).unwrap(),
            1.10400
        );
        // SELL mirrored: SL above, TP below.
        assert_eq!(
            sl_tp_price("sell", 1.10000, 20.0, 5, true).unwrap(),
            1.10200
        );
        assert_eq!(
            sl_tp_price("sell", 1.10000, 40.0, 5, false).unwrap(),
            1.09600
        );
        // Non-FX digits: gold-style 2 decimals (pip = 0.1, so 5 pips = 0.50).
        assert_eq!(sl_tp_price("buy", 2400.00, 5.0, 2, true).unwrap(), 2399.50);
        // Zero pips rejected.
        assert!(sl_tp_price("buy", 1.1, 0.0, 5, true).is_err());
    }

    #[test]
    fn registry_lookup_matches_dispatch_arms() {
        // Every registry name must be routable — guards against a typo in
        // either the table or the execute() match.
        for c in BRIDGE_COMMANDS {
            assert!(super::super::lookup(c.name).is_some());
        }
    }
}
