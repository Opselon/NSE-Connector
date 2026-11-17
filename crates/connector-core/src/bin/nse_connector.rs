//! NSE Connector daemon — the bridge between the NSE Platform (server side)
//! and the local MetaTrader terminal (client side).
//!
//! Boot: load secrets → validate key → register device → MT5 handshake +
//! manifest discovery → open the platform WebSocket. Runtime: platform
//! `command` frames run the full local gate chain ([`ActionDispatcher`]) and
//! answer with `action.result` frames; `sync_request` frames and the periodic
//! tick push bounded account/position/history batches; `ack` frames retire
//! delivered batches. Ctrl+C shuts down gracefully.
//!
//! Configuration is environment-first; secrets can instead live in the OS
//! credential store via the `store-secrets` subcommand, so no secret ever
//! sits in a file or shell history.
//!
//! ```text
//! NSE Platform ──WSS──▶ nse-connector.exe ──HTTP──▶ MetaTrader terminal
//!   (commands, acks)        (this process)            (MCP, local only)
//! ```

use std::path::PathBuf;
use std::time::Duration;

use connector_core::actions::dispatcher::{ActionDispatcher, DispatchContext};
use connector_core::actions::kill::KillSwitch;
use connector_core::actions::manifest::{ActionManifest, EXPECTED_ACTION_COUNT};
use connector_core::actions::risk::RiskTier;
use connector_core::bridge;
use connector_core::mcp::{McpClientConfig, McpHttpClient, trading_tools};
use connector_core::nse::session;
use connector_core::nse::{
    CommandJournal, ManagedSocket, NseGatewayClient, NseGatewayConfig, NseSocketConfig,
};
use connector_core::security::secrets::{Secret, SecretStore, WindowsCredentialStore};
use connector_core::sync::{SyncConfig, SyncEngine};

const CREDENTIAL_SERVICE: &str = "NSE Connector";
const STATE_DIR: &str = "nse-connector";
const DEFAULT_PLATFORM_URL: &str = "https://bridge.nexusverse.space";
const DEFAULT_MT5_ENDPOINT: &str = "http://127.0.0.1:22346/mcp";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("store-secrets") => store_secrets(),
        Some("--help") | Some("-h") | Some("help") => print_help(),
        _ => {
            init_tracing();
            let code = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(run());
            std::process::exit(code);
        }
    }
}

fn print_help() {
    println!(
        "NSE Connector — bridge between the NSE Platform and the local MetaTrader terminal\n\
         \n\
         Usage:\n\
           nse-connector                 Run the bridge daemon\n\
           nse-connector store-secrets   Persist NSE_API_KEY / MT5_MCP_TOKEN from the\n\
                                         environment into the OS credential store\n\
         \n\
         Configuration (environment):\n\
           NSE_PLATFORM_URL        Platform base URL (default {DEFAULT_PLATFORM_URL})\n\
           NSE_API_KEY             Platform API key (or stored under '{CREDENTIAL_SERVICE}')\n\
           NSE_DEVICE_ID           Device identity (else generated + persisted under %APPDATA%)\n\
           MT5_MCP_URL             Local MT5 MCP endpoint (default {DEFAULT_MT5_ENDPOINT})\n\
           MT5_MCP_TOKEN           Local MT5 MCP bearer token (optional if the server has none)\n\
           NSE_SYNC_INTERVAL_SECS  Periodic sync cadence (default 60)\n\
           RUST_LOG                Log filter (default info)"
    );
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// Persists secrets from the environment into the OS credential store so the
/// daemon runs without them in env/shell history. Values are never printed.
fn store_secrets() {
    let store = WindowsCredentialStore::new(CREDENTIAL_SERVICE);
    let mut stored = 0usize;
    for (env_key, entry) in [
        ("NSE_API_KEY", "nse_api_key"),
        ("MT5_MCP_TOKEN", "mt5_mcp_token"),
    ] {
        let Ok(value) = std::env::var(env_key) else {
            println!("skip {entry}: {env_key} not set");
            continue;
        };
        if value.trim().is_empty() {
            println!("skip {entry}: {env_key} is empty");
            continue;
        }
        match store.store(entry, &Secret::new(value.trim())) {
            Ok(()) => {
                println!("stored {entry} -> OS credential store ({CREDENTIAL_SERVICE})");
                stored += 1;
            }
            Err(e) => {
                eprintln!("failed to store {entry}: {e}");
                std::process::exit(1);
            }
        }
    }
    if stored == 0 {
        eprintln!("nothing stored — set NSE_API_KEY and/or MT5_MCP_TOKEN first");
        std::process::exit(1);
    }
}

struct DaemonConfig {
    platform_url: String,
    api_key: Secret,
    device_id: String,
    mt5_endpoint: String,
    mt5_bearer: Option<Secret>,
    sync_interval: Duration,
}

/// Env-first resolution; secrets fall back to the OS credential store.
fn load_config() -> Result<DaemonConfig, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let secret = |env_key: &str, entry: &str| -> Result<Option<Secret>, String> {
        if let Some(v) = env(env_key) {
            return Ok(Some(Secret::new(v)));
        }
        WindowsCredentialStore::new(CREDENTIAL_SERVICE)
            .load(entry)
            .map_err(|e| format!("credential store read failed for {entry}: {e}"))
    };

    let platform_url = env("NSE_PLATFORM_URL").unwrap_or_else(|| DEFAULT_PLATFORM_URL.to_string());
    let api_key = secret("NSE_API_KEY", "nse_api_key")?.ok_or_else(|| {
        "NSE_API_KEY missing — set it in the environment or run `nse-connector store-secrets`"
            .to_string()
    })?;
    let device_id = resolve_device_id()?;
    let mt5_endpoint = env("MT5_MCP_URL").unwrap_or_else(|| DEFAULT_MT5_ENDPOINT.to_string());
    let mt5_bearer = secret("MT5_MCP_TOKEN", "mt5_mcp_token")?;
    let sync_interval = Duration::from_secs(
        env("NSE_SYNC_INTERVAL_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(60)
            .clamp(5, 3600),
    );
    Ok(DaemonConfig {
        platform_url,
        api_key,
        device_id,
        mt5_endpoint,
        mt5_bearer,
        sync_interval,
    })
}

/// Device identity: env override, else a persisted UUIDv7 under
/// `%APPDATA%/nse-connector/device_id` (stable across restarts).
fn resolve_device_id() -> Result<String, String> {
    if let Some(id) = std::env::var("NSE_DEVICE_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        return Ok(id.trim().to_string());
    }
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create state dir {}: {e}", dir.display()))?;
    let path = dir.join("device_id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    let id = uuid::Uuid::now_v7().to_string();
    std::fs::write(&path, &id).map_err(|e| format!("cannot persist device id: {e}"))?;
    Ok(id)
}

fn state_dir() -> Result<PathBuf, String> {
    let base = std::env::var("APPDATA").map_err(|_| "APPDATA is not set".to_string())?;
    Ok(PathBuf::from(base).join(STATE_DIR))
}

async fn run() -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            return 2;
        }
    };
    tracing::info!(
        device_id = %cfg.device_id,
        platform = %cfg.platform_url,
        mt5 = %cfg.mt5_endpoint,
        sync_interval_secs = cfg.sync_interval.as_secs(),
        "NSE Connector starting"
    );

    // ---- Platform (server side) REST gateway --------------------------------
    let gateway = match NseGatewayClient::new(NseGatewayConfig {
        base_url: cfg.platform_url.clone(),
        api_key: cfg.api_key.clone(),
        device_id: cfg.device_id.clone(),
        ..Default::default()
    }) {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("invalid platform configuration: {e}");
            return 2;
        }
    };

    let validation = match gateway.validate_key().await {
        Ok(v) if v.valid => v,
        Ok(_) => {
            tracing::error!("platform rejected the API key (valid=false)");
            return 3;
        }
        Err(e) => {
            tracing::error!("key validation failed: {e}");
            return 3;
        }
    };
    let ent = validation.entitlements.clone();
    tracing::info!(
        history_sync = ent.history_sync,
        position_manager = ent.position_manager,
        ai_coach = ent.ai_coach,
        auto_trade = ent.auto_trade,
        "platform entitlements accepted (server is authoritative)"
    );
    match gateway.register_device().await {
        Ok(r) if r.registered => tracing::info!("device registered with the platform"),
        Ok(_) => {
            tracing::error!("device registration refused");
            return 3;
        }
        Err(e) => {
            tracing::error!("device registration failed: {e}");
            return 3;
        }
    }

    // ---- Terminal (client side) MCP handshake + manifest --------------------
    let state = std::sync::Arc::new(bridge::BridgeState::new());
    let mut mcp = match McpHttpClient::new(McpClientConfig {
        endpoint: cfg.mt5_endpoint.clone(),
        bearer: cfg.mt5_bearer.clone(),
        ..Default::default()
    }) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("MT5 client configuration error: {e}");
            return 2;
        }
    };
    let handshake = match mcp.connect().await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("MT5 MCP handshake failed (is the terminal running?): {e}");
            return 4;
        }
    };
    tracing::info!(
        protocol = %handshake.protocol_version,
        server = %handshake.server_name,
        "MT5 handshake established"
    );
    state.set_mt5_connected(true, Some(handshake.clone()));
    let tools = match mcp.list_tools().await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("MT5 tool discovery failed: {e}");
            return 4;
        }
    };
    if let Err(e) = McpHttpClient::verify_required_tools(&tools, &trading_tools::REQUIRED) {
        tracing::error!("terminal is missing required tools: {e}");
        return 4;
    }
    let manifest = match ActionManifest::discover(&mut mcp).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("action manifest discovery failed: {e}");
            return 4;
        }
    };
    let count = manifest.verify_count(EXPECTED_ACTION_COUNT);
    tracing::info!(
        actions = count.actual,
        expected = count.expected,
        verdict = count.verdict,
        "action manifest discovered"
    );
    for tier in [
        RiskTier::ReadOnly,
        RiskTier::LocalModification,
        RiskTier::FilesystemOrCode,
        RiskTier::Financial,
    ] {
        tracing::info!(tier = tier.as_str(), actions = manifest.by_tier(tier).len());
    }

    // ---- Local safety gates + dispatch context ------------------------------
    // §4 "no duplicate command execution after reconnect": the persistent
    // journal gates every dispatch. The bridge refuses to run without it.
    let journal_path = match state_dir() {
        Ok(d) => d.join("executed_commands.jsonl"),
        Err(e) => {
            tracing::error!("state directory unavailable: {e}");
            return 2;
        }
    };
    let journal = match CommandJournal::open_with_capacity(journal_path, 10_000) {
        Ok(j) => std::sync::Arc::new(j),
        Err(e) => {
            tracing::error!("command journal unavailable — refusing to run: {e}");
            return 2;
        }
    };
    let kill = std::sync::Arc::new(KillSwitch::new());
    let dispatcher = ActionDispatcher::new(manifest, kill.clone());
    let mut ctx = DispatchContext::new(cfg.device_id.clone());
    if let Some(u) = validation.user_id.clone() {
        ctx = ctx.with_user(u);
    }
    // Entitlement → permission mapping (server is authoritative, spec §8).
    if ent.position_manager {
        ctx = ctx.with_grant("local.modify");
    }
    if ent.ai_coach {
        ctx = ctx.with_grant("advanced.execute");
    }
    if ent.auto_trade {
        ctx = ctx.with_grant("trading.execute");
    }
    ctx = ctx.with_financial_consent(ent.auto_trade);

    // ---- WebSocket bridge + sync engine -------------------------------------
    let ws_url = format!(
        "{}/api/v1/connector/ws",
        cfg.platform_url
            .trim_end_matches('/')
            .replace("https://", "wss://")
            .replace("http://", "ws://")
    );
    let mut socket = ManagedSocket::spawn(NseSocketConfig {
        url: ws_url,
        api_key: cfg.api_key.clone(),
        device_id: cfg.device_id.clone(),
        ..Default::default()
    });
    let mut engine = SyncEngine::new(SyncConfig::default());
    let mut sync_tick = tokio::time::interval(cfg.sync_interval);
    sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Tell the Platform what manifest this connector runs (as-built
    // `manifest.update`); the server acks and can flag drift.
    let manifest_frame = session::manifest_update_frame(&dispatcher.manifest());
    if socket
        .outbound
        .send(manifest_frame.to_string())
        .await
        .is_err()
    {
        tracing::warn!("manifest.update could not be queued — will not retry this session");
    }
    tracing::info!("bridge is up — listening for platform frames");

    let exit_code = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown signal received");
                break 0;
            }
            _ = sync_tick.tick() => {
                let mut deps = session::SessionDeps {
                    mcp: &mut mcp,
                    ctx: &mut ctx,
                    dispatcher: &dispatcher,
                    engine: &mut engine,
                    gateway: &gateway,
                    state: &state,
                    journal: &journal,
                    outbound: &socket.outbound,
                };
                session::sync_cycle(&mut deps).await;
            }
            frame = socket.inbound.recv() => {
                let Some(text) = frame else {
                    tracing::warn!("bridge socket closed");
                    break 5;
                };
                let mut deps = session::SessionDeps {
                    mcp: &mut mcp,
                    ctx: &mut ctx,
                    dispatcher: &dispatcher,
                    engine: &mut engine,
                    gateway: &gateway,
                    state: &state,
                    journal: &journal,
                    outbound: &socket.outbound,
                };
                session::handle_platform_frame(&text, &mut deps).await;
            }
        }
    };

    socket.shutdown().await;
    tracing::info!(pending_batches = engine.pending_count(), "bridge stopped");
    exit_code
}
