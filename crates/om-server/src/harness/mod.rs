//! Concrete agent harness adapters and Phase 0 registration helpers (W0-6).
//!
//! - [`MockHarness`]: fast, deterministic in-process harness for CI, demos, and E2E tests
//!   without requiring external API keys or binaries.
//! - [`opencode`]: `opencode acp` launcher & binary/version probe wrapping [`crate::acp::AcpHarness`].
//! - [`register_phase0_harnesses`] / [`default_phase0_registry`]: registers Phase 0 harnesses
//!   into [`crate::orchestrator::HarnessRegistry`].

pub mod mock;
pub mod opencode;

pub use mock::{MockHarness, MockHarnessConfig};
pub use opencode::{OpenCodeConfig, OpenCodeProbe, opencode_harness, probe_opencode};

use crate::orchestrator::HarnessRegistry;
use om_core::harness::HarnessCapabilities;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Public metadata describing a registered harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessDescriptor {
    pub id: String,
    pub capabilities: HarnessCapabilities,
}

/// Registers Phase 0 default harnesses (`mock` and `opencode`) into `registry`.
pub fn register_phase0_harnesses(registry: &mut HarnessRegistry) {
    registry.register(Arc::new(MockHarness::new()));
    registry.register(Arc::new(opencode_harness(OpenCodeConfig::from_env())));
}

/// Builds a [`HarnessRegistry`] populated with Phase 0 defaults (`mock` and `opencode`).
pub fn default_phase0_registry() -> HarnessRegistry {
    let mut registry = HarnessRegistry::new();
    register_phase0_harnesses(&mut registry);
    registry
}

/// Lists descriptors (`id`, `capabilities`) for every harness in `registry`.
pub fn describe_harnesses(registry: &HarnessRegistry) -> Vec<HarnessDescriptor> {
    registry
        .ids()
        .into_iter()
        .filter_map(|id| {
            let h = registry.get(&id)?;
            Some(HarnessDescriptor {
                id,
                capabilities: h.capabilities(),
            })
        })
        .collect()
}
