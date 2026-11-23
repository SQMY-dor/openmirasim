//! OpenMirasim central server library.
//!
//! Module ownership follows `docs/roadmap.md` §3:
//! - hark: `db`, `acp`, `orchestrator`, `sandbox`, `workspace`, `approval`, `auth`, `proxy`, `scheduler`
//! - Antigravity: `api`, `harness`, `pty`

pub mod acp;
pub mod api;
pub mod approval;
pub mod auth;
pub mod db;
pub mod harness;
pub mod orchestrator;
pub mod proxy;
pub mod sandbox;
pub mod stream;
pub mod workspace;
