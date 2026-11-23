//! Translation between ACP wire JSON and `om-core` types.
//!
//! Parsing is deliberately lenient: unknown update kinds or content types are skipped or
//! downgraded to text, so a newer agent never breaks the session.

use om_core::approval::{ApprovalOption, ApprovalOptionKind};
use om_core::content::{ContentBlock, Prompt};
use om_core::event::{
    PlanEntry, PlanEntryStatus, ToolCallInfo, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use om_core::harness::HarnessEvent;
use om_core::session::StopReason;
use serde_json::{Value, json};

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn enum_field<T: serde::de::DeserializeOwned>(v: &Value, key: &str) -> Option<T> {
    v.get(key)
        .filter(|x| !x.is_null())
        .and_then(|x| serde_json::from_value(x.clone()).ok())
}

fn non_null(v: &Value, key: &str) -> Option<Value> {
    v.get(key).filter(|x| !x.is_null()).cloned()
}

/// ACP `ContentBlock` -> om-core [`ContentBlock`].
pub fn content_from_acp(v: &Value) -> Option<ContentBlock> {
    match v.get("type").and_then(Value::as_str)? {
        "text" => Some(ContentBlock::Text {
            text: str_field(v, "text").unwrap_or_default(),
        }),
        "resource_link" => Some(ContentBlock::ResourceLink {
            uri: str_field(v, "uri")?,
            name: str_field(v, "name").unwrap_or_default(),
            mime_type: str_field(v, "mimeType"),
        }),
        "resource" => {
            let res = v.get("resource")?;
            match str_field(res, "text") {
                Some(text) => Some(ContentBlock::Text { text }),
                None => Some(ContentBlock::ResourceLink {
                    uri: str_field(res, "uri")?,
                    name: str_field(res, "uri").unwrap_or_default(),
                    mime_type: str_field(res, "mimeType"),
                }),
            }
        }
        other => Some(ContentBlock::Text {
            text: format!("[{other} content omitted]"),
        }),
    }
}

/// om-core [`Prompt`] -> ACP `ContentBlock[]`.
pub fn prompt_to_acp(prompt: &Prompt) -> Value {
    Value::Array(
        prompt
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => json!({"type": "text", "text": text}),
                ContentBlock::ResourceLink {
                    uri,
                    name,
                    mime_type,
                } => {
                    let mut v = json!({"type": "resource_link", "uri": uri, "name": name});
                    if let Some(m) = mime_type {
                        v["mimeType"] = json!(m);
                    }
                    v
                }
            })
            .collect(),
    )
}

/// ACP `ToolCallContent` -> om-core [`ContentBlock`].
fn tool_content_from_acp(v: &Value) -> Option<ContentBlock> {
    match v.get("type").and_then(Value::as_str)? {
        "content" => content_from_acp(v.get("content")?),
        "diff" => Some(ContentBlock::Text {
            text: format!("[diff] {}", str_field(v, "path").unwrap_or_default()),
        }),
        "terminal" => Some(ContentBlock::Text {
            text: format!(
                "[terminal] {}",
                str_field(v, "terminalId").unwrap_or_default()
            ),
        }),
        _ => None,
    }
}

fn tool_contents(v: &Value) -> Vec<ContentBlock> {
    v.get("content")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(tool_content_from_acp).collect())
        .unwrap_or_default()
}

fn locations(v: &Value) -> Vec<String> {
    v.get("locations")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|l| str_field(l, "path")).collect())
        .unwrap_or_default()
}

/// ACP `tool_call` update -> [`ToolCallInfo`].
pub fn tool_call_from_acp(v: &Value) -> Option<ToolCallInfo> {
    Some(ToolCallInfo {
        tool_call_id: str_field(v, "toolCallId")?,
        title: str_field(v, "title").unwrap_or_default(),
        kind: enum_field::<ToolKind>(v, "kind").unwrap_or_default(),
        status: enum_field::<ToolCallStatus>(v, "status").unwrap_or_default(),
        raw_input: non_null(v, "rawInput"),
        locations: locations(v),
    })
}

/// ACP `tool_call_update` -> [`ToolCallUpdate`].
pub fn tool_call_update_from_acp(v: &Value) -> Option<ToolCallUpdate> {
    Some(ToolCallUpdate {
        tool_call_id: str_field(v, "toolCallId")?,
        status: enum_field(v, "status"),
        title: str_field(v, "title"),
        content: tool_contents(v),
        raw_output: non_null(v, "rawOutput"),
    })
}

fn plan_from_acp(v: &Value) -> Vec<PlanEntry> {
    v.get("entries")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|e| PlanEntry {
                    content: str_field(e, "content").unwrap_or_default(),
                    status: enum_field::<PlanEntryStatus>(e, "status").unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ACP `session/update` params.update -> harness event (None for ignored kinds).
pub fn session_update_to_event(update: &Value) -> Option<HarnessEvent> {
    match update.get("sessionUpdate").and_then(Value::as_str)? {
        "agent_message_chunk" => {
            content_from_acp(update.get("content")?).map(HarnessEvent::MessageChunk)
        }
        "agent_thought_chunk" => {
            content_from_acp(update.get("content")?).map(HarnessEvent::ThoughtChunk)
        }
        "tool_call" => tool_call_from_acp(update).map(HarnessEvent::ToolCall),
        "tool_call_update" => tool_call_update_from_acp(update).map(HarnessEvent::ToolCallUpdate),
        "plan" => Some(HarnessEvent::Plan(plan_from_acp(update))),
        // user_message_chunk (we record prompts ourselves), available_commands_update,
        // current_mode_update, config options, ... are not part of the Phase 0 stream.
        _ => None,
    }
}

/// ACP `PermissionOption[]` -> om-core options.
pub fn permission_options_from_acp(v: &Value) -> Vec<ApprovalOption> {
    v.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|o| {
                    Some(ApprovalOption {
                        option_id: str_field(o, "optionId")?,
                        name: str_field(o, "name").unwrap_or_default(),
                        kind: enum_field::<ApprovalOptionKind>(o, "kind")?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ACP `stopReason` -> [`StopReason`].
pub fn stop_reason_from_acp(v: &Value) -> StopReason {
    match v.get("stopReason").and_then(Value::as_str) {
        Some("end_turn") => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("max_turn_requests") => StopReason::MaxTurnRequests,
        Some("refusal") => StopReason::Refusal,
        Some("cancelled") => StopReason::Cancelled,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_message_chunk() {
        let ev = session_update_to_event(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hi"}
        }));
        assert_eq!(
            ev,
            Some(HarnessEvent::MessageChunk(ContentBlock::Text {
                text: "hi".into()
            }))
        );
    }

    #[test]
    fn maps_tool_call_and_update() {
        let ev = session_update_to_event(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "c1",
            "title": "Run tests",
            "kind": "execute",
            "status": "pending",
            "rawInput": {"cmd": "cargo test"},
            "locations": [{"path": "/w/a.rs", "line": 3}]
        }))
        .unwrap();
        let HarnessEvent::ToolCall(info) = ev else {
            panic!()
        };
        assert_eq!(info.kind, ToolKind::Execute);
        assert_eq!(info.locations, vec!["/w/a.rs".to_owned()]);

        let ev = session_update_to_event(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "c1",
            "status": "completed",
            "content": [
                {"type": "content", "content": {"type": "text", "text": "ok"}},
                {"type": "diff", "path": "/w/a.rs", "newText": "x"}
            ]
        }))
        .unwrap();
        let HarnessEvent::ToolCallUpdate(u) = ev else {
            panic!()
        };
        assert_eq!(u.status, Some(ToolCallStatus::Completed));
        assert_eq!(u.content.len(), 2);
        assert_eq!(u.title, None);
    }

    #[test]
    fn unknown_updates_and_kinds_are_tolerated() {
        assert_eq!(
            session_update_to_event(&json!({"sessionUpdate": "current_mode_update"})),
            None
        );
        let ev = session_update_to_event(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "c", "title": "t", "kind": "warp"
        }))
        .unwrap();
        let HarnessEvent::ToolCall(info) = ev else {
            panic!()
        };
        assert_eq!(info.kind, ToolKind::Other);
    }

    #[test]
    fn maps_permission_options_and_stop_reason() {
        let opts = permission_options_from_acp(&json!([
            {"optionId": "a", "name": "Allow", "kind": "allow_once"},
            {"optionId": "b", "name": "Bogus", "kind": "maybe"}
        ]));
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].kind, ApprovalOptionKind::AllowOnce);
        assert_eq!(
            stop_reason_from_acp(&json!({"stopReason": "cancelled"})),
            StopReason::Cancelled
        );
    }

    #[test]
    fn prompt_roundtrip_shape() {
        let v = prompt_to_acp(&Prompt::text("go"));
        assert_eq!(v, json!([{"type": "text", "text": "go"}]));
    }
}
