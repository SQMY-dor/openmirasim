//! Generic ACP (Agent Client Protocol) harness (W0-3).
//!
//! Speaks ACP v1 over the agent's stdio (newline-delimited JSON-RPC 2.0). Any agent
//! that implements ACP — `opencode acp` in Phase 0, `dsh` / `cc` / `codexcli` adapters in
//! Phase 1 — is driven by the same [`AcpHarness`], configured only with its command line.
//!
//! The client advertises no `fs` or `terminal` capabilities: agents use their own file
//! system and process access, which the [`om_core::sandbox::Sandbox`] confines.

mod harness;
pub mod mapping;
pub mod rpc;

pub use harness::{AcpHarness, AcpHarnessOptions};

/// ACP major protocol version this client implements.
pub const ACP_PROTOCOL_VERSION: u64 = 1;
