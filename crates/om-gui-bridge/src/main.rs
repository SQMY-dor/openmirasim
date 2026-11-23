//! Standalone `om-gui-bridge` daemon entry point (W2-5).

use clap::{Parser, ValueEnum};
use om_gui_bridge::{BridgeConfig, DriverKind, GuiBridge};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliDriver {
    Auto,
    X11,
    Simulated,
}

impl From<CliDriver> for DriverKind {
    fn from(v: CliDriver) -> Self {
        match v {
            CliDriver::Auto => DriverKind::Auto,
            CliDriver::X11 => DriverKind::X11,
            CliDriver::Simulated => DriverKind::Simulated,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "om-gui-bridge",
    version,
    about = "OpenMirasim isolated external Antigravity GUI bridge (Xvfb + per-member profiles + noVNC takeover)"
)]
struct Cli {
    /// TCP bind address (loopback recommended).
    #[arg(long, env = "OM_GUI_BRIDGE_BIND", default_value = "127.0.0.1:4941")]
    bind: SocketAddr,

    /// Root directory for per-member isolated GUI profile directories (chmod 0700).
    #[arg(
        long,
        env = "OM_GUI_PROFILE_ROOT",
        default_value = "./data/gui-profiles"
    )]
    profile_root: PathBuf,

    /// Maximum concurrent GUI sessions on this host.
    #[arg(long, env = "OM_GUI_MAX_SESSIONS", default_value_t = 4)]
    max_sessions: usize,

    /// Idle session eviction timeout in seconds.
    #[arg(long, env = "OM_GUI_IDLE_TIMEOUT_SECS", default_value_t = 1800)]
    idle_timeout_secs: u64,

    /// GUI driver backend (`auto`, `x11`, or `simulated`).
    #[arg(long, env = "OM_GUI_DRIVER", value_enum, default_value_t = CliDriver::Auto)]
    driver: CliDriver,

    /// Path to the Antigravity GUI executable (for `x11` mode).
    #[arg(long, env = "OM_ANTIGRAVITY_GUI_BIN", default_value = "antigravity")]
    gui_binary: String,

    /// Optional shared Bearer token required on `/v1/*` bridge control endpoints.
    #[arg(long, env = "OM_GUI_BRIDGE_TOKEN")]
    token: Option<String>,

    /// Optional public base URL advertised in noVNC takeover links.
    #[arg(long, env = "OM_GUI_PUBLIC_URL")]
    public_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let mut cfg = BridgeConfig::new(cli.profile_root);
    cfg.max_sessions = cli.max_sessions;
    cfg.idle_timeout = Duration::from_secs(cli.idle_timeout_secs);
    cfg.driver = cli.driver.into();
    cfg.gui_binary = cli.gui_binary;
    cfg.bridge_token = cli.token.filter(|t| !t.trim().is_empty());
    cfg.public_base_url = cli.public_url.filter(|u| !u.trim().is_empty());

    let (bridge, handle) = GuiBridge::start(cfg, cli.bind).await?;
    tracing::info!(addr = %bridge.addr(), "om-gui-bridge listening");

    tokio::signal::ctrl_c().await?;
    handle.abort();
    tracing::info!("om-gui-bridge stopped");
    Ok(())
}
