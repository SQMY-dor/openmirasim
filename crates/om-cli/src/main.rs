//! OpenMirasim CLI / scriptable `ctl` control plane / Ratatui `tui` client.

mod ctl;
mod tui;

use clap::{Parser, Subcommand};
use serde_json::json;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "om-cli",
    version = om_core::crate_version(),
    about = "OpenMirasim command-line control plane (`ctl`) and Ratatui terminal client (`tui`)"
)]
struct Cli {
    #[command(subcommand)]
    command: TopCommand,
}

#[derive(Debug, Subcommand)]
enum TopCommand {
    /// Scriptable pure-JSON control plane for `om-server`.
    Ctl(ctl::CtlArgs),
    /// Interactive Ratatui multi-session dashboard with direct `/ws/pty` terminal attach.
    Tui(tui::TuiArgs),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        TopCommand::Ctl(args) => {
            let pretty = args.pretty;
            match ctl::run(args).await {
                Ok(val) => {
                    let rendered = if pretty {
                        serde_json::to_string_pretty(&val)
                    } else {
                        serde_json::to_string(&val)
                    };
                    match rendered {
                        Ok(s) => {
                            println!("{s}");
                            ExitCode::SUCCESS
                        }
                        Err(e) => {
                            eprintln!("{}", json!({ "error": e.to_string() }));
                            ExitCode::FAILURE
                        }
                    }
                }
                Err(err) => {
                    let out = json!({
                        "error": err.message,
                        "status": err.status,
                        "detail": err.body,
                    });
                    eprintln!("{out}");
                    ExitCode::FAILURE
                }
            }
        }
        TopCommand::Tui(args) => match tui::run(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("om-cli tui error: {err}");
                ExitCode::FAILURE
            }
        },
    }
}
