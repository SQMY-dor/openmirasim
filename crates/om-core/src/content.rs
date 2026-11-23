//! Content blocks carried by prompts and agent messages.
//!
//! The shape intentionally mirrors ACP `ContentBlock` (text and resource links) so the
//! ACP harness can translate without loss, while staying small enough for Phase 0.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One block of user or agent content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ContentBlock {
    /// Plain or Markdown text.
    Text { text: String },
    /// A reference to a file or URL the agent can read.
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        mime_type: Option<String>,
    },
}

/// A prompt submitted to an agent.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Prompt {
    pub content: Vec<ContentBlock>,
}

impl Prompt {
    /// Builds a prompt consisting of a single text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Returns `true` when the prompt has no non-empty content.
    pub fn is_empty(&self) -> bool {
        self.content.iter().all(|b| match b {
            ContentBlock::Text { text } => text.trim().is_empty(),
            ContentBlock::ResourceLink { uri, .. } => uri.is_empty(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_block_uses_type_tag() {
        let json = serde_json::to_value(ContentBlock::Text { text: "hi".into() }).unwrap();
        assert_eq!(json, serde_json::json!({"type": "text", "text": "hi"}));
    }

    #[test]
    fn empty_prompt_detection() {
        assert!(Prompt::text("   ").is_empty());
        assert!(Prompt::default().is_empty());
        assert!(!Prompt::text("go").is_empty());
    }
}
