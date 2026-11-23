//! Shared domain models, wire event contracts (`ts-rs`), and `Harness`/`Sandbox` traits
//! for OpenMirasim.
//!
//! Phase 0 Contract Owner (W0-1): hark (Opus 5.5).

/// Returns the crate version string for bootstrap diagnostics.
pub const fn protocol_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_non_empty() {
        assert!(!protocol_version().is_empty());
    }
}
