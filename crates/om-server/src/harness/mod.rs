//! Concrete agent harness adapters and runtime registry (W0-6).
//!
//! - [`MockHarness`]: fast, deterministic in-process harness for CI, demos, and E2E tests
//!   without requiring external API keys or binaries.
//! - [`opencode`]: `opencode acp` launcher & binary/version probe wrapping [`crate::acp::AcpHarness`].
//! - [`HarnessRegistry`]: thread-safe lookup table for registered harnesses.

pub mod mock;
pub mod opencode;

pub use mock::{MockHarness, MockHarnessConfig};
pub use opencode::{OpenCodeConfig, OpenCodeProbe, opencode_harness, probe_opencode};

use om_core::harness::{Harness, HarnessCapabilities};
use om_core::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// Public metadata describing a registered harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessDescriptor {
    pub id: String,
    pub capabilities: HarnessCapabilities,
}

/// Thread-safe registry of configured [`Harness`] factories.
#[derive(Clone, Default)]
pub struct HarnessRegistry {
    inner: Arc<RwLock<BTreeMap<String, Arc<dyn Harness>>>>,
}

impl HarnessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a registry populated with Phase 0 defaults (`mock` and `opencode`).
    pub fn with_phase0_defaults() -> Self {
        let reg = Self::new();
        reg.register(Arc::new(MockHarness::new()));
        reg.register(Arc::new(opencode_harness(OpenCodeConfig::from_env())));
        reg
    }

    /// Registers or replaces a harness under its [`Harness::id`].
    pub fn register(&self, harness: Arc<dyn Harness>) {
        if let Ok(mut map) = self.inner.write() {
            map.insert(harness.id().to_string(), harness);
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Harness>> {
        self.inner.read().ok().and_then(|map| map.get(id).cloned())
    }

    pub fn require(&self, id: &str) -> CoreResult<Arc<dyn Harness>> {
        self.get(id)
            .ok_or_else(|| CoreError::NotFound(format!("harness '{id}' is not registered")))
    }

    pub fn list(&self) -> Vec<HarnessDescriptor> {
        self.inner
            .read()
            .map(|map| {
                map.values()
                    .map(|h| HarnessDescriptor {
                        id: h.id().to_string(),
                        capabilities: h.capabilities(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
