//! Concrete agent harness adapters and runtime registry (W0-6 & W1-4).
//!
//! - [`MockHarness`]: fast, deterministic in-process harness for CI, demos, and E2E tests.
//! - [`opencode`]: `opencode acp` launcher & binary/version probe wrapping [`crate::acp::AcpHarness`].
//! - [`dsh`]: DeepSeek Harness (`dsh acp`) ACP wrapper & binary/version probe.
//! - [`cc`]: Claude Code (`claude -p --output-format stream-json`) NDJSON adapter & probe.
//! - [`codexcli`]: Codex CLI (`codex exec --json`) NDJSON adapter & probe.
//! - [`HarnessRegistry`]: thread-safe lookup table for registered harnesses.

pub mod cc;
pub mod codexcli;
pub mod dsh;
pub mod mock;
pub mod opencode;
pub mod probe;
pub mod stream_cli;

pub use cc::{ClaudeCodeConfig, ClaudeCodeHarness, cc_harness, probe_cc};
pub use codexcli::{CodexCliConfig, CodexCliHarness, codexcli_harness, probe_codexcli};
pub use dsh::{DshConfig, dsh_harness, probe_dsh};
pub use mock::{MockHarness, MockHarnessConfig};
pub use opencode::{OpenCodeConfig, OpenCodeProbe, opencode_harness, probe_opencode};
pub use probe::CliProbe;

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

/// Live host availability report for one harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessProbeReport {
    pub id: String,
    pub available: bool,
    pub version: Option<String>,
    pub binary_path: Option<String>,
    pub error: Option<String>,
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

    /// Creates a registry populated with all Phase 1 headless CLI harnesses:
    /// `mock`, `opencode`, `dsh`, `cc`, and `codexcli`.
    ///
    /// When `proxy_enabled` is `true`, raw host model API keys are NOT forwarded into
    /// adapter `CommandSpec::env` so [`crate::proxy::KeyProxy`] remains the sole credential boundary.
    pub fn with_phase1_defaults(proxy_enabled: bool) -> Self {
        let reg = Self::new();
        reg.register(Arc::new(MockHarness::new()));
        let oc_cfg = if proxy_enabled {
            OpenCodeConfig::from_env_for_proxy()
        } else {
            OpenCodeConfig::from_env()
        };
        let dsh_cfg = if proxy_enabled {
            DshConfig::from_env_for_proxy()
        } else {
            DshConfig::from_env()
        };
        let cc_cfg = if proxy_enabled {
            ClaudeCodeConfig::from_env_for_proxy()
        } else {
            ClaudeCodeConfig::from_env()
        };
        let codex_cfg = if proxy_enabled {
            CodexCliConfig::from_env_for_proxy()
        } else {
            CodexCliConfig::from_env()
        };
        reg.register(Arc::new(opencode_harness(oc_cfg)));
        reg.register(Arc::new(dsh_harness(dsh_cfg)));
        reg.register(Arc::new(cc_harness(cc_cfg)));
        reg.register(Arc::new(codexcli_harness(codex_cfg)));
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

/// Probes live host binary availability and version for every harness in `registry`.
pub async fn probe_registered_harnesses(registry: &HarnessRegistry) -> Vec<HarnessProbeReport> {
    let descriptors = registry.list();
    let mut reports = Vec::with_capacity(descriptors.len());
    for desc in descriptors {
        let report = match desc.id.as_str() {
            "mock" => HarnessProbeReport {
                id: desc.id,
                available: true,
                version: Some(format!("builtin-{}", om_core::crate_version())),
                binary_path: None,
                error: None,
                capabilities: desc.capabilities,
            },
            "opencode" => {
                let p = probe_opencode(&OpenCodeConfig::from_env_for_proxy().binary).await;
                HarnessProbeReport {
                    id: desc.id,
                    available: p.available,
                    version: p.version,
                    binary_path: p.binary_path.map(|b| b.to_string_lossy().into_owned()),
                    error: p.error,
                    capabilities: desc.capabilities,
                }
            }
            "dsh" => {
                let p = probe_dsh(&DshConfig::from_env_for_proxy().binary).await;
                HarnessProbeReport {
                    id: desc.id,
                    available: p.available,
                    version: p.version,
                    binary_path: p.binary_path.map(|b| b.to_string_lossy().into_owned()),
                    error: p.error,
                    capabilities: desc.capabilities,
                }
            }
            "cc" => {
                let p = probe_cc(&ClaudeCodeConfig::from_env_for_proxy().binary).await;
                HarnessProbeReport {
                    id: desc.id,
                    available: p.available,
                    version: p.version,
                    binary_path: p.binary_path.map(|b| b.to_string_lossy().into_owned()),
                    error: p.error,
                    capabilities: desc.capabilities,
                }
            }
            "codexcli" => {
                let p = probe_codexcli(&CodexCliConfig::from_env_for_proxy().binary).await;
                HarnessProbeReport {
                    id: desc.id,
                    available: p.available,
                    version: p.version,
                    binary_path: p.binary_path.map(|b| b.to_string_lossy().into_owned()),
                    error: p.error,
                    capabilities: desc.capabilities,
                }
            }
            _ => HarnessProbeReport {
                id: desc.id,
                available: true,
                version: None,
                binary_path: None,
                error: None,
                capabilities: desc.capabilities,
            },
        };
        reports.push(report);
    }
    reports
}
