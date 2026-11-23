//! OpenMirasim central Linux server entry point (`om-server`).
//!
//! Environment variables:
//! - `OM_BIND`: TCP listen address (default `127.0.0.1:4939`)
//! - `OM_DATA_DIR`: directory holding `openmirasim.db` (default `./data`)
//! - `OM_TOKEN`: static Phase 0 Bearer token (required on non-loopback; auto-generated on loopback if unset)
//! - `OM_ALLOW_DIRECT_SANDBOX`: `1`/`true` to permit `SandboxMode::Direct` (refused unless `OM_BIND` is loopback)
//! - `OM_STATIC_DIR`: optional compiled Web frontend directory (auto-detects `apps/web/dist` when present)
//! - `OM_CORS_ORIGIN`: optional allowed CORS origin (e.g. `http://127.0.0.1:5173`)
//! - `OM_SPACE_NAME`: default space name (default `default`)
//! - `OM_FAKE_AGENT_BIN`: optional path to `om-fake-acp-agent` to register `"fake-acp"` harness

use om_core::sandbox::CommandSpec;
use om_server::acp::AcpHarness;
use om_server::api::{ApiConfig, AppState, build_router};
use om_server::db::Db;
use om_server::harness::default_phase0_registry;
use om_server::orchestrator::{Orchestrator, OrchestratorConfig};
use std::net::SocketAddr;
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

fn resolve_static_dir() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("OM_STATIC_DIR") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let default_dist = PathBuf::from("apps/web/dist");
    if default_dist.join("index.html").is_file() {
        Some(default_dist)
    } else {
        None
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

    let auth_token = match std::env::var("OM_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
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

    let mut registry = default_phase0_registry();
    if let Ok(fake_bin) = std::env::var("OM_FAKE_AGENT_BIN")
        && !fake_bin.trim().is_empty()
    {
        registry.register(Arc::new(AcpHarness::new(
            "fake-acp",
            CommandSpec::new(fake_bin.trim()),
        )));
    }

    let orchestrator = Orchestrator::new(
        db,
        registry,
        OrchestratorConfig {
            allow_direct_sandbox,
            ..Default::default()
        },
    );

    let recovered = orchestrator.recover().await?;
    if !recovered.is_empty() {
        tracing::info!(
            count = recovered.len(),
            "recovered live sessions after restart"
        );
    }

    let static_dir = resolve_static_dir();
    let allow_cors_origin = std::env::var("OM_CORS_ORIGIN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

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
        "om-server listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("om-server shut down cleanly");
    Ok(())
}
