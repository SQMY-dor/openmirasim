//! Model provider contract for the host key proxy (W2-2).
//!
//! Agents never see upstream API keys. Each sandboxed session gets a short-lived
//! session token and the proxy's loopback base URL; the proxy swaps the token for the
//! real key on the host. These types describe providers to clients without any secret.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// How a provider expects its key, and which environment variables agents read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ProviderKind {
    /// Anthropic Messages API: key in `x-api-key`.
    /// Agents read `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY`.
    Anthropic,
    /// OpenAI or any OpenAI-compatible API (DeepSeek, OpenRouter, vLLM, ...): key in
    /// `Authorization: Bearer`. Agents read `OPENAI_BASE_URL` / `OPENAI_API_KEY`.
    OpenAi,
}

impl ProviderKind {
    /// Default `(base_url_env, api_key_env)` agents of this kind read.
    pub fn default_env(self) -> (&'static str, &'static str) {
        match self {
            Self::Anthropic => ("ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY"),
            Self::OpenAi => ("OPENAI_BASE_URL", "OPENAI_API_KEY"),
        }
    }
}

/// A configured upstream provider, as shown to members (no key, no upstream URL).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProviderInfo {
    /// Path segment and stable id, e.g. `anthropic`, `deepseek`.
    pub id: String,
    pub kind: ProviderKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_env_names() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::OpenAi).unwrap(),
            "\"open_ai\""
        );
        assert_eq!(
            ProviderKind::Anthropic.default_env(),
            ("ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY")
        );
    }
}
