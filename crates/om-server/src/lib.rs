//! OpenMirasim central server library.
//!
//! Module ownership follows `docs/roadmap.md` §3:
//! - hark: `db`, `acp`, `orchestrator`, `sandbox`, `workspace`, `approval`, `auth`, `proxy`, `scheduler`
//! - Antigravity: `api`, `harness`, `pty`

pub mod acp;
pub mod db;
pub mod stream;
