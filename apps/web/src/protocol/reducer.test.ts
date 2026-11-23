import { describe, expect, it } from "vitest";
import { computeBackoffMs } from "./client";
import {
  applySessionEvent,
  applySessionEvents,
  createEmptyProjection,
} from "./reducer";
import type { EventPayload, SessionEvent } from "./types";

const TS = 1_763_865_900_000;

function ev(
  seq: number,
  payload: EventPayload,
  sessionId = "s1"
): SessionEvent {
  return { session_id: sessionId, seq, ts: TS, payload };
}

describe("om-core SessionEvent projection (ts-rs contract)", () => {
  it("projects harness_ready, plan, streaming chunks, tool_call updates, and turn_ended", () => {
    const initial = createEmptyProjection("s1", "starting");
    const next = applySessionEvents(initial, [
      ev(1, {
        type: "harness_ready",
        capabilities: {
          native_approval: true,
          steer: true,
          resume: true,
          structured_tools: true,
          pty_attach: false,
          remote_screen: false,
        },
        native_session_id: "acp_123",
      }),
      ev(2, { type: "status_changed", from: "starting", to: "idle" }),
      ev(3, { type: "status_changed", from: "idle", to: "running" }),
      ev(4, {
        type: "prompt_submitted",
        turn_id: "t1",
        member_id: "alice",
        prompt: { content: [{ type: "text", text: "hello" }] },
      }),
      ev(5, {
        type: "plan",
        turn_id: "t1",
        entries: [
          { content: "inspect", status: "completed" },
          { content: "reply", status: "in_progress" },
        ],
      }),
      ev(6, {
        type: "agent_thought_chunk",
        turn_id: "t1",
        content: { type: "text", text: "thinking " },
      }),
      ev(7, {
        type: "tool_call",
        turn_id: "t1",
        tool_call: {
          tool_call_id: "c1",
          title: "Read README.md",
          kind: "read",
          status: "in_progress",
          raw_input: { path: "README.md" },
          locations: ["README.md"],
        },
      }),
      ev(8, {
        type: "tool_call_update",
        turn_id: "t1",
        update: {
          tool_call_id: "c1",
          status: "completed",
          content: [{ type: "text", text: "ok" }],
        },
      }),
      ev(9, {
        type: "agent_message_chunk",
        turn_id: "t1",
        content: { type: "text", text: "Hi " },
      }),
      ev(10, {
        type: "agent_message_chunk",
        turn_id: "t1",
        content: { type: "text", text: "there!" },
      }),
      ev(11, { type: "turn_ended", turn_id: "t1", stop_reason: "end_turn" }),
    ]);

    expect(next.lastSeq).toBe(11);
    expect(next.status).toBe("idle");
    expect(next.nativeSessionId).toBe("acp_123");
    expect(next.capabilities?.native_approval).toBe(true);
    expect(next.items).toHaveLength(2);

    const assistant = next.items[1];
    expect(assistant.kind).toBe("assistant");
    if (assistant.kind === "assistant") {
      expect(assistant.plan).toHaveLength(2);
      expect(assistant.thinking).toBe("thinking ");
      expect(assistant.text).toBe("Hi there!");
      expect(assistant.completed).toBe(true);
      expect(assistant.toolCalls[0].status).toBe("completed");
      expect(assistant.toolCalls[0].outputText).toBe("ok");
    }
  });

  it("deduplicates overlapping events during catchup + live WS overlap", () => {
    let state = createEmptyProjection("s1");
    state = applySessionEvents(state, [
      ev(1, {
        type: "agent_message_chunk",
        turn_id: "t1",
        content: { type: "text", text: "A" },
      }),
      ev(2, {
        type: "agent_message_chunk",
        turn_id: "t1",
        content: { type: "text", text: "B" },
      }),
    ]);

    state = applySessionEvent(
      state,
      ev(2, {
        type: "agent_message_chunk",
        turn_id: "t1",
        content: { type: "text", text: "B" },
      })
    );
    state = applySessionEvent(
      state,
      ev(3, {
        type: "agent_message_chunk",
        turn_id: "t1",
        content: { type: "text", text: "C" },
      })
    );

    expect(state.lastSeq).toBe(3);
    expect(state.dedupedCount).toBe(1);
    const assistant = state.items[0];
    if (assistant.kind === "assistant") {
      expect(assistant.text).toBe("ABC");
    }
  });

  it("tracks ACP approval options and resolution transitions", () => {
    let state = createEmptyProjection("s1", "running");
    state = applySessionEvent(
      state,
      ev(1, {
        type: "approval_requested",
        turn_id: "t1",
        approval: {
          id: "a1",
          session_id: "s1",
          tool_call_id: "c1",
          title: "Run deploy",
          options: [
            { option_id: "allow_once", name: "允许一次", kind: "allow_once" },
            { option_id: "reject_once", name: "拒绝", kind: "reject_once" },
          ],
          status: "pending",
          created_at: TS,
        },
      })
    );

    expect(state.status).toBe("waiting_approval");

    state = applySessionEvent(
      state,
      ev(2, {
        type: "approval_resolved",
        approval_id: "a1",
        decision: { outcome: "selected", option_id: "allow_once" },
        resolved_by: "alice",
      })
    );

    expect(state.status).toBe("running");
    const assistant = state.items[0];
    if (assistant.kind === "assistant") {
      expect(assistant.approvals[0].status).toBe("resolved");
      expect(assistant.approvals[0].decision).toEqual({
        outcome: "selected",
        option_id: "allow_once",
      });
      expect(assistant.approvals[0].resolved_by).toBe("alice");
    }
  });

  it("detects sequence gaps and ignores events from other sessions", () => {
    let state = createEmptyProjection("s1");
    state = applySessionEvent(
      state,
      ev(
        1,
        {
          type: "agent_message_chunk",
          turn_id: "t1",
          content: { type: "text", text: "hello" },
        },
        "s1"
      )
    );
    state = applySessionEvent(
      state,
      ev(
        2,
        {
          type: "agent_message_chunk",
          turn_id: "t1",
          content: { type: "text", text: "other" },
        },
        "s2"
      )
    );
    expect(state.lastSeq).toBe(1);

    state = applySessionEvent(
      state,
      ev(
        4,
        {
          type: "agent_message_chunk",
          turn_id: "t1",
          content: { type: "text", text: " world" },
        },
        "s1"
      )
    );
    expect(state.lastSeq).toBe(4);
    expect(state.gapDetectedCount).toBe(1);
  });

  it("caps reconnect backoff", () => {
    expect(computeBackoffMs(1)).toBe(600);
    expect(computeBackoffMs(2)).toBeGreaterThan(600);
    expect(computeBackoffMs(20)).toBe(12_000);
  });
});
