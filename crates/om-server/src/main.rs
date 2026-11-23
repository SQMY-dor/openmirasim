//! OpenMirasim central Linux server entry point (`om-server`).
//!
//! Environment variables:
//! - `OM_BIND`: TCP listen address (default `127.0.0.1:4939`)
//! - `OM_DATA_DIR`: directory holding `openmirasim.db`, worktrees, scratch dirs, and homes (default `./data`)
//! - `OM_TOKEN`: static Bearer token (required on non-loopback; auto-generated on loopback if unset)
//! - `OM_ALLOW_DIRECT_SANDBOX`: `1`/`true` to permit `SandboxMode::Direct` (refused unless `OM_BIND` is loopback)
//! - `OM_REQUIRE_BWRAP`: `1`/`true` to fail startup if `bubblewrap` probing fails (default: fail when direct is disabled)
//! - `OM_ENABLE_KEY_PROXY`: `1`/`true` to enable loopback [`KeyProxy`] using host provider keys and suppress raw key forwarding
//! - `OM_STATIC_DIR`: optional compiled Web frontend directory (auto-detects `apps/web/dist` when present)
//! - `OM_CORS_ORIGIN`: optional allowed CORS origin (e.g. `http://127.0.0.1:5173`)
//! - `OM_SPACE_NAME`: default space name (default `default`)
//! - `OM_FAKE_AGENT_BIN`: optional path to `om-fake-acp-agent` to register `"fake-acp"` harness

use om_core::proxy::ProviderKind;
use om_core::sandbox::CommandSpec;
use om_server::acp::AcpHarness;
use om_server::api::{ApiConfig, AppState, build_router};
use om_server::db::Db;
use om_server::harness::HarnessRegistry;
use om_server::orchestrator::{Orchestrator, OrchestratorConfig};
use om_server::proxy::{KeyProxy, ProviderConfig, ProxyConfig};
use om_server::sandbox::BwrapSandbox;
use om_server::workspace::{WorkspaceConfig, WorkspaceManager};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

fn env_flag(key: &str) -> bool {
    matches!(
        std::env::var(key).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn resolve_static_dir() -> Option<PathBuf> {
    if let Some(trimmed) = env_non_empty("OM_STATIC_DIR") {
        return Some(PathBuf::from(trimmed));
    }
    let default_dist = PathBuf::from("apps/web/dist");
    if default_dist.join("index.html").is_file() {
        Some(default_dist)
    } else {
        None
    }
}

fn build_proxy_config_from_env() -> ProxyConfig {
    let mut providers = Vec::new();
    if let Some(key) = env_non_empty("ANTHROPIC_API_KEY") {
        let upstream = env_non_empty("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|| "https://api.anthropic.com".into());
        providers.push(ProviderConfig::new(
            "anthropic",
            ProviderKind::Anthropic,
            upstream,
            key,
        ));
    }
    if let Some(key) = env_non_empty("OPENAI_API_KEY") {
        let upstream =
            env_non_empty("OPENAI_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".into());
        providers.push(ProviderConfig::new(
            "openai",
            ProviderKind::OpenAi,
            upstream,
            key,
        ));
    }
    if let Some(key) = env_non_empty("DEEPSEEK_API_KEY") {
        let upstream =
            env_non_empty("DEEPSEEK_BASE_URL").unwrap_or_else(|| "https://api.deepseek.com".into());
        providers.push(
            ProviderConfig::new("deepseek", ProviderKind::OpenAi, upstream, key)
                .with_env("DEEPSEEK_BASE_URL", "DEEPSEEK_API_KEY"),
        );
    }
    if let Some(key) = env_non_empty("OPENROUTER_API_KEY") {
        providers.push(
            ProviderConfig::new(
                "openrouter",
                ProviderKind::OpenAi,
                "https://openrouter.ai/api/v1",
                key,
            )
            .with_env("OPENROUTER_BASE_URL", "OPENROUTER_API_KEY"),
        );
    }
    ProxyConfig {
        providers,
        ..Default::default()
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sigterm.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let bind_raw = std::env::var("OM_BIND").unwrap_or_else(|_| "127.0.0.1:4939".into());
    let bind_addr: SocketAddr = bind_raw
        .parse()
        .map_err(|e| format!("invalid OM_BIND `{bind_raw}`: {e}"))?;

    let allow_direct_sandbox = env_flag("OM_ALLOW_DIRECT_SANDBOX");
    if allow_direct_sandbox && !bind_addr.ip().is_loopback() {
        return Err(
            "refusing to start: OM_ALLOW_DIRECT_SANDBOX=1 is only permitted on loopback binds"
                .into(),
        );
    }

    let auth_token = match env_non_empty("OM_TOKEN") {
        Some(tok) => tok,
        None if bind_addr.ip().is_loopback() => {
            let generated = uuid::Uuid::now_v7().to_string();
            eprintln!("OM_TOKEN not set; generated ephemeral loopback token: {generated}");
            generated
        }
        None => {
            return Err("refusing to start: OM_TOKEN must be set when binding non-loopback".into());
        }
    };

    let data_dir = PathBuf::from(std::env::var("OM_DATA_DIR").unwrap_or_else(|_| "./data".into()));
    let db_path = data_dir.join("openmirasim.db");
    let db = Db::open(&db_path)?;

    let space_name = std::env::var("OM_SPACE_NAME").unwrap_or_else(|_| "default".into());
    let default_space_id = db.ensure_space(&space_name).await?;

    let workspaces = WorkspaceManager::new(db.clone(), WorkspaceConfig::new(&data_dir))?;

    let enable_key_proxy = env_flag("OM_ENABLE_KEY_PROXY");
    let mut proxy_task = None;
    let key_proxy = if enable_key_proxy {
        let proxy_cfg = build_proxy_config_from_env();
        let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let (proxy, task) = KeyProxy::start(proxy_cfg, loopback).await?;
        proxy_task = Some(task);
        Some(proxy)
    } else {
        None
    };

    let registry = HarnessRegistry::with_phase1_defaults(key_proxy.is_some());
    if let Some(fake_bin) = env_non_empty("OM_FAKE_AGENT_BIN") {
        registry.register(Arc::new(AcpHarness::new(
            "fake-acp",
            CommandSpec::new(fake_bin),
        )));
    }

    let mut orchestrator = Orchestrator::new(
        db,
        registry,
        OrchestratorConfig {
            allow_direct_sandbox,
            ..Default::default()
        },
    )
    .with_workspaces(workspaces);

    if let Some(proxy) = key_proxy {
        orchestrator = orchestrator.with_key_proxy(proxy);
    }

    let require_bwrap = env_flag("OM_REQUIRE_BWRAP") || !allow_direct_sandbox;
    let bwrap_enabled = match BwrapSandbox::default().probe().await {
        Ok(sb) => {
            orchestrator = orchestrator.with_bwrap_sandbox(Arc::new(sb));
            true
        }
        Err(e) if require_bwrap => {
            return Err(format!("refusing to start without working bwrap sandbox: {e}").into());
        }
        Err(e) => {
            tracing::warn!(error = %e, "bwrap sandbox unavailable; only direct sandbox sessions will work");
            false
        }
    };

    let recovered = orchestrator.recover().await?;
    if !recovered.is_empty() {
        tracing::info!(
            count = recovered.len(),
            "recovered live sessions after restart"
        );
    }

    let static_dir = resolve_static_dir();
    let allow_cors_origin = env_non_empty("OM_CORS_ORIGIN");

    let state = AppState {
        orchestrator,
        config: ApiConfig {
            auth_token,
            default_space_id,
            static_dir: static_dir.clone(),
            allow_cors_origin,
        },
    };

    let app = build_router(state);
    let listener = TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(
        addr = %local_addr,
        db = %db_path.display(),
        static_dir = ?static_dir,
        allow_direct_sandbox,
        bwrap_enabled,
        key_proxy = proxy_task.is_some(),
        "om-server listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    if let Some(task) = proxy_task {
        task.abort();
    }
    tracing::info!("om-server shut down cleanly");
    Ok(())
}
