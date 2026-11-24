//! OpenMirasim central Linux server entry point (`om-server`).
//!
//! Environment variables:
//! - `OM_BIND`: TCP listen address (default `127.0.0.1:4939`)
//! - `OM_DATA_DIR`: directory holding `openmirasim.db`, worktrees, scratch dirs, push subscriptions, and homes (default `./data`)
//! - `OM_TOKEN`: optional static Owner bootstrap Bearer token (omit on production deployments to use pure `omt_...` RBAC device tokens)
//! - `OM_OWNER_NAME`: display name for the initial bootstrapped `Owner` member (default `Owner`)
//! - `OM_ALLOW_DIRECT_SANDBOX`: `1`/`true` to permit `SandboxMode::Direct` (refused unless `OM_BIND` is loopback)
//! - `OM_REQUIRE_BWRAP`: `1`/`true` to fail startup if `bubblewrap` probing fails (default: fail when direct is disabled)
//! - `OM_ENABLE_KEY_PROXY`: `1`/`true` to force-enable or `0`/`false` to disable loopback [`KeyProxy`] (default: auto-enabled whenever any upstream provider API key is detected in the environment)
//! - `OM_CGROUPS`: optional cgroups v2 mode for `bwrap` (`user` or `system`)
//! - `OM_MEMORY_MAX_MB` / `OM_CPU_QUOTA_PERCENT` / `OM_PIDS_MAX`: optional default sandbox resource limits when `OM_CGROUPS` is enabled
//! - `OM_ALLOW_LOOPBACK_PUSH`: `1`/`true` to allow loopback/localhost Web Push endpoints in local dev/test (default: disabled)
//! - `OM_GUI_BRIDGE_URL`: optional external `om-gui-bridge` URL (e.g. `http://127.0.0.1:4941`) registering the `antigravity-gui` harness
//! - `OM_GUI_BRIDGE_TOKEN`: optional Bearer token for authenticating to `om-gui-bridge`
//! - `OM_VAPID_PUBLIC_KEY`: optional VAPID public key advertised to PWA clients for Web Push
//! - `OM_STATIC_DIR`: optional compiled Web frontend directory (auto-detects `apps/web/dist` when present)
//! - `OM_CORS_ORIGIN`: optional allowed CORS origin (e.g. `http://127.0.0.1:5173`)
//! - `OM_SPACE_NAME`: default space name (default `default`)
//! - `OM_FAKE_AGENT_BIN`: optional path to `om-fake-acp-agent` to register `"fake-acp"` harness

use om_core::proxy::ProviderKind;
use om_core::sandbox::{CommandSpec, ResourceLimits};
use om_server::acp::AcpHarness;
use om_server::api::{ApiConfig, ApiExtensions, AppState, build_router_with_extensions};
use om_server::auth::Auth;
use om_server::db::Db;
use om_server::harness::{
    GuiBridgeConfig, GuiBridgeHarness, HarnessRegistry, RemoteScreenRegistry,
};
use om_server::orchestrator::{Orchestrator, OrchestratorConfig};
use om_server::proxy::{KeyProxy, ProviderConfig, ProxyConfig};
use om_server::pty::{PtyConfig, PtyManager};
use om_server::sandbox::{BwrapConfig, BwrapSandbox, CgroupMode};
use om_server::scheduler::{Dispatcher, GoalRunner, Scheduler};
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

fn env_opt_bool(key: &str) -> Option<bool> {
    let raw = std::env::var(key).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn env_u64(key: &str) -> Result<Option<u64>, String> {
    let Some(raw) = env_non_empty(key) else {
        return Ok(None);
    };
    raw.parse::<u64>()
        .map(Some)
        .map_err(|e| format!("invalid {key} `{raw}`: {e}"))
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

fn build_bwrap_config_from_env() -> Result<BwrapConfig, String> {
    let mut cfg = BwrapConfig::default();
    if let Some(mode) = env_non_empty("OM_CGROUPS") {
        cfg.cgroups = match mode.to_ascii_lowercase().as_str() {
            "user" | "systemd-user" => CgroupMode::SystemdUserScope,
            "system" | "systemd-system" => CgroupMode::SystemdSystemScope,
            "off" | "disabled" | "none" => CgroupMode::Disabled,
            other => {
                return Err(format!(
                    "invalid OM_CGROUPS `{other}` (expected `user`, `system`, or `disabled`)"
                ));
            }
        };
    }
    let memory_max_bytes = env_u64("OM_MEMORY_MAX_MB")?.map(|mb| mb.saturating_mul(1024 * 1024));
    let cpu_quota_percent = env_u64("OM_CPU_QUOTA_PERCENT")?
        .map(|v| u32::try_from(v).map_err(|_| "OM_CPU_QUOTA_PERCENT out of range".to_string()))
        .transpose()?;
    let pids_max = env_u64("OM_PIDS_MAX")?
        .map(|v| u32::try_from(v).map_err(|_| "OM_PIDS_MAX out of range".to_string()))
        .transpose()?;
    cfg.default_limits = ResourceLimits {
        memory_max_bytes,
        cpu_quota_percent,
        pids_max,
    };
    Ok(cfg)
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

    let data_dir = PathBuf::from(std::env::var("OM_DATA_DIR").unwrap_or_else(|_| "./data".into()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !data_dir.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&data_dir)?;
        }
        let _ = std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&data_dir)?;
    }

    let db_path = data_dir.join("openmirasim.db");
    let db = Db::open(&db_path)?;

    let space_name = std::env::var("OM_SPACE_NAME").unwrap_or_else(|_| "default".into());
    let default_space_id = db.ensure_space(&space_name).await?;

    // Bootstrap the first Owner member & device token on a brand-new database.
    let auth_svc = Auth::new(db.clone());
    let owner_name = env_non_empty("OM_OWNER_NAME").unwrap_or_else(|| "Owner".into());
    if let Some((owner, issued)) = auth_svc
        .bootstrap_owner(&default_space_id, &owner_name, "bootstrap-owner")
        .await?
    {
        eprintln!(
            "Bootstrapped initial Owner `{}` ({}). One-time device token (save now, never printed again):\n  {}",
            owner.display_name, owner.id, issued.secret
        );
    }

    // Static OM_TOKEN is optional: when omitted, pure RBAC device tokens (`omt_...`) are used.
    let auth_token = env_non_empty("OM_TOKEN").unwrap_or_default();

    let workspaces = WorkspaceManager::new(db.clone(), WorkspaceConfig::new(&data_dir))?;

    // Auto-enable loopback KeyProxy whenever any upstream provider API key is present in the
    // environment so raw upstream secrets never enter agent subprocesses by default.
    let proxy_cfg = build_proxy_config_from_env();
    let has_upstream_keys = !proxy_cfg.providers.is_empty();
    let enable_key_proxy = match env_opt_bool("OM_ENABLE_KEY_PROXY") {
        Some(true) => true,
        Some(false) => {
            if has_upstream_keys && !bind_addr.ip().is_loopback() {
                return Err(
                    "refusing to start: OM_ENABLE_KEY_PROXY=0 with upstream API keys is only permitted on loopback binds"
                        .into(),
                );
            }
            if has_upstream_keys {
                tracing::warn!(
                    "OM_ENABLE_KEY_PROXY=0 explicitly set on loopback; raw upstream API keys will be forwarded to harnesses"
                );
            }
            false
        }
        None => {
            if has_upstream_keys {
                tracing::info!(
                    providers = proxy_cfg.providers.len(),
                    "detected upstream provider API keys; enabling loopback KeyProxy automatically"
                );
                true
            } else {
                false
            }
        }
    };

    let mut proxy_task = None;
    let key_proxy = if enable_key_proxy {
        let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let (proxy, task) = KeyProxy::start(proxy_cfg, loopback).await?;
        proxy_task = Some(task);
        Some(proxy)
    } else {
        None
    };

    let registry = HarnessRegistry::with_phase1_defaults(key_proxy.is_some());
    if let Some(fake_bin) = env_non_empty("OM_FAKE_AGENT_BIN") {
        let mut fake_spec = CommandSpec::new(fake_bin);
        fake_spec.clear_env = true;
        if let Ok(path) = std::env::var("PATH") {
            fake_spec = fake_spec.env("PATH", path);
        }
        registry.register(Arc::new(AcpHarness::new("fake-acp", fake_spec)));
    }

    let remote_screens = RemoteScreenRegistry::new();
    let gui_bridge_enabled = if let Some(gui_cfg) = GuiBridgeConfig::from_env() {
        tracing::info!(url = %gui_cfg.base_url, "registering external antigravity-gui bridge harness");
        registry.register(Arc::new(GuiBridgeHarness::new(
            gui_cfg,
            remote_screens.clone(),
        )));
        true
    } else {
        false
    };

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

    let bwrap_cfg = build_bwrap_config_from_env()?;
    let require_bwrap = env_flag("OM_REQUIRE_BWRAP") || !allow_direct_sandbox;
    let bwrap_enabled = match BwrapSandbox::new(bwrap_cfg).probe().await {
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

    let scheduler = Scheduler::new(orchestrator.clone());
    let goal_runner = GoalRunner::new(orchestrator.clone());
    let dispatcher = Dispatcher::new(orchestrator.clone());
    let recovered_goals = goal_runner.recover().await?;
    if recovered_goals > 0 {
        tracing::info!(
            count = recovered_goals,
            "marked stale active goals as stopped after restart"
        );
    }
    let scheduler_task = scheduler.spawn();

    let static_dir = resolve_static_dir();
    let allow_cors_origin = env_non_empty("OM_CORS_ORIGIN");
    let vapid_public_key = env_non_empty("OM_VAPID_PUBLIC_KEY");
    let allow_loopback_push = env_flag("OM_ALLOW_LOOPBACK_PUSH");

    let state = AppState {
        orchestrator: orchestrator.clone(),
        config: ApiConfig {
            auth_token,
            default_space_id,
            static_dir: static_dir.clone(),
            allow_cors_origin,
        },
    };
    let pty_manager = PtyManager::new(PtyConfig {
        recordings_dir: Some(data_dir.join("pty")),
        ..Default::default()
    });
    let extensions = ApiExtensions {
        remote_screens,
        push_store_path: Some(data_dir.join("push-subscriptions.json")),
        vapid_public_key,
        allow_loopback_push,
        pty: Some(pty_manager.clone()),
        scheduler: Some(scheduler.clone()),
        goals: Some(goal_runner),
        dispatcher: Some(dispatcher),
        hub_store_path: Some(data_dir.join("hub-registry.json")),
    };

    let app = build_router_with_extensions(state, extensions);
    let listener = TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(
        addr = %local_addr,
        db = %db_path.display(),
        static_dir = ?static_dir,
        allow_direct_sandbox,
        bwrap_enabled,
        key_proxy = proxy_task.is_some(),
        gui_bridge = gui_bridge_enabled,
        "om-server listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    scheduler.shutdown();
    scheduler_task.abort();
    pty_manager.shutdown_all();
    orchestrator.shutdown_all().await;
    if let Some(task) = proxy_task {
        task.abort();
    }
    tracing::info!("om-server shut down cleanly");
    Ok(())
}
