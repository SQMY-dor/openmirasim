//! Scripted ACP agent used by OpenMirasim tests and the Phase 0 E2E suite (no API key).
//!
//! Behaviour is chosen by the prompt text:
//! - contains `tool`  : reports a tool call and asks permission; result depends on the decision
//! - contains `slow`  : streams chunks every 50 ms until cancelled
//! - contains `crash` : exits with status 3 mid-turn
//! - anything else    : streams `echo: <text>` plus a thought and a plan, then `end_turn`
//!
//! `FAKE_ACP_VERSION` overrides the advertised protocol version.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Agent {
    out: Mutex<std::io::Stdout>,
    cancelled: AtomicBool,
    waiters: Mutex<HashMap<String, Sender<Value>>>,
    next_id: AtomicU64,
}

impl Agent {
    fn send(&self, v: Value) {
        let mut out = self.out.lock().unwrap();
        writeln!(out, "{v}").unwrap();
        out.flush().unwrap();
    }

    fn update(&self, session: &str, update: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session, "update": update}
        }));
    }

    fn chunk(&self, session: &str, kind: &str, text: &str) {
        self.update(
            session,
            json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}),
        );
    }

    fn ask_permission(&self, session: &str) -> Value {
        let id = format!("perm-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = channel();
        self.waiters.lock().unwrap().insert(id.clone(), tx);
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/request_permission",
            "params": {
                "sessionId": session,
                "toolCall": {"toolCallId": "call-1"},
                "options": [
                    {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"}
                ]
            }
        }));
        rx.recv().unwrap_or(Value::Null)
    }

    fn run_prompt(&self, id: Value, session: String, text: String) {
        self.cancelled.store(false, Ordering::SeqCst);
        let stop = if text.contains("crash") {
            self.chunk(&session, "agent_message_chunk", "about to crash");
            std::process::exit(3);
        } else if text.contains("slow") {
            let mut stop = "end_turn";
            for i in 0..200 {
                if self.cancelled.load(Ordering::SeqCst) {
                    stop = "cancelled";
                    break;
                }
                self.chunk(&session, "agent_message_chunk", &format!("tick {i} "));
                std::thread::sleep(Duration::from_millis(50));
            }
            stop
        } else if text.contains("tool") {
            self.update(
                &session,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-1",
                    "title": "rm -rf build",
                    "kind": "execute",
                    "status": "pending",
                    "rawInput": {"command": "rm -rf build"}
                }),
            );
            let answer = self.ask_permission(&session);
            match answer.pointer("/outcome/outcome").and_then(Value::as_str) {
                Some("selected")
                    if answer.pointer("/outcome/optionId") == Some(&json!("allow")) =>
                {
                    self.update(
                        &session,
                        json!({
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": "call-1",
                            "status": "completed",
                            "content": [{"type": "content", "content": {"type": "text", "text": "removed"}}]
                        }),
                    );
                    self.chunk(&session, "agent_message_chunk", "ran it");
                    "end_turn"
                }
                Some("selected") => {
                    self.update(
                        &session,
                        json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "failed"}),
                    );
                    self.chunk(&session, "agent_message_chunk", "skipped");
                    "end_turn"
                }
                _ => "cancelled",
            }
        } else {
            self.chunk(&session, "agent_thought_chunk", "thinking");
            self.update(
                &session,
                json!({"sessionUpdate": "plan", "entries": [
                    {"content": "reply", "priority": "high", "status": "in_progress"}
                ]}),
            );
            self.chunk(&session, "agent_message_chunk", "echo: ");
            self.chunk(&session, "agent_message_chunk", &text);
            "end_turn"
        };
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": stop}}));
    }
}

fn main() {
    let agent = Arc::new(Agent {
        out: Mutex::new(std::io::stdout()),
        cancelled: AtomicBool::new(false),
        waiters: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(0),
    });
    let version: u64 = std::env::var("FAKE_ACP_VERSION")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    eprintln!("fake acp agent started");

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => agent.send(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": version,
                    "agentCapabilities": {"loadSession": true},
                    "agentInfo": {"name": "fake", "version": "0.0.0"},
                    "authMethods": []
                }
            })),
            Some("session/new") => agent.send(json!({
                "jsonrpc": "2.0", "id": id, "result": {"sessionId": "fake-session-1"}
            })),
            Some("session/load") => {
                let sid = params["sessionId"].as_str().unwrap_or_default().to_owned();
                agent.chunk(&sid, "user_message_chunk", "old prompt");
                agent.chunk(&sid, "agent_message_chunk", "old reply");
                agent.send(json!({"jsonrpc": "2.0", "id": id, "result": null}));
            }
            Some("session/prompt") => {
                let sid = params["sessionId"].as_str().unwrap_or_default().to_owned();
                let text = params["prompt"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                let agent = agent.clone();
                let id = id.unwrap_or(Value::Null);
                std::thread::spawn(move || agent.run_prompt(id, sid, text));
            }
            Some("session/cancel") => agent.cancelled.store(true, Ordering::SeqCst),
            Some(_) => {
                if let Some(id) = id {
                    agent.send(json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"}
                    }));
                }
            }
            None => {
                // A response to one of our requests.
                let key = id
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_default();
                let waiter = agent.waiters.lock().unwrap().remove(&key);
                if let Some(tx) = waiter {
                    let _ = tx.send(msg.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }
}
