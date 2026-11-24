import { describe, expect, it } from "vitest";
import { computeProfileId, normalizeServerUrl } from "../desktop/tauriBridge";
import { computeBackoffMs } from "./client";
import {
  applySessionEvent,
  applySessionEvents,
  createEmptyProjection,
} from "./reducer";
import { isSafeHttpUrl, type EventPayload, type SessionEvent } from "./types";

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
          steer: false,
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
      ev(12, { type: "status_changed", from: "running", to: "idle" }),
    ]);

    expect(next.lastSeq).toBe(12);
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

  it("tracks ACP approvals, auto_rule attribution, and authoritative status_changed", () => {
    let state = createEmptyProjection("s1", "running");
    state = applySessionEvents(state, [
      ev(1, {
        type: "approval_requested",
        turn_id: "t1",
        approval: {
          id: "a1",
          session_id: "s1",
          tool_call_id: "c1",
          tool_kind: "read",
          locations: ["/ws/README.md"],
          title: "Read README.md",
          options: [
            { option_id: "allow_once", name: "允许一次", kind: "allow_once" },
            { option_id: "reject_once", name: "拒绝", kind: "reject_once" },
          ],
          status: "pending",
          created_at: TS,
        },
      }),
      // Auto-resolved approval never transitions to waiting_approval
      ev(2, {
        type: "approval_resolved",
        approval_id: "a1",
        decision: { outcome: "selected", option_id: "allow_once" },
        auto_rule: "read_in_worktree",
      }),
    ]);

    expect(state.status).toBe("running");
    const assistant = state.items[0];
    if (assistant.kind === "assistant") {
      expect(assistant.approvals[0].status).toBe("resolved");
      expect(assistant.approvals[0].auto_rule).toBe("read_in_worktree");
      expect(assistant.approvals[0].tool_kind).toBe("read");
      expect(assistant.approvals[0].locations).toEqual(["/ws/README.md"]);
    }
  });

  it("projects W2-3 queue/steer and W4-1 goal/dispatch events", () => {
    const state = applySessionEvents(createEmptyProjection("s1", "running"), [
      ev(1, {
        type: "prompt_queued",
        position: 0,
        queued: {
          id: "q1",
          member_id: "alice",
          prompt: { content: [{ type: "text", text: "first queued" }] },
          mode: "queue",
          created_at: TS,
        },
      }),
      ev(2, {
        type: "prompt_queued",
        position: 1,
        queued: {
          id: "q2",
          member_id: "bob",
          prompt: { content: [{ type: "text", text: "second queued" }] },
          mode: "queue",
          created_at: TS + 1,
        },
      }),
      ev(3, { type: "queue_reordered", order: ["q2", "q1"] }),
      ev(4, { type: "queue_paused", paused: true }),
      ev(5, {
        type: "queue_item_removed",
        queued_id: "q1",
        reason: { reason: "withdrawn" },
      }),
      ev(6, {
        type: "prompt_steered",
        turn_id: "t1",
        member_id: "alice",
        prompt: { content: [{ type: "text", text: "switch to rust" }] },
      }),
      ev(7, {
        type: "goal_updated",
        goal: {
          id: "g1",
          space_id: "sp1",
          session_id: "s1",
          created_by: "alice",
          objective: "Make all tests pass",
          done_marker: "[[goal:done]]",
          max_turns: 5,
          turns_used: 1,
          status: "active",
          created_at: TS,
          updated_at: TS + 2,
        },
      }),
      ev(8, {
        type: "dispatch_sent",
        dispatch: {
          id: "d1",
          space_id: "sp1",
          from_session: "s1",
          to_session: "s2",
          depth: 1,
          created_session: false,
          member_id: "alice",
          submit: { outcome: "started", turn_id: "t2" },
          created_at: TS + 3,
        },
      }),
      ev(9, {
        type: "dispatch_received",
        dispatch: {
          id: "d2",
          space_id: "sp1",
          from_session: "s0",
          to_session: "s1",
          depth: 1,
          created_session: true,
          member_id: "bob",
          submit: { outcome: "queued", queued_id: "q3", position: 0 },
          created_at: TS + 4,
        },
      }),
    ]);

    expect(state.queue.map((q) => q.id)).toEqual(["q2"]);
    expect(state.queuePaused).toBe(true);
    expect(state.goal?.status).toBe("active");
    expect(state.goal?.turns_used).toBe(1);
    expect(state.items.map((i) => i.kind)).toEqual([
      "user",
      "goal",
      "dispatch",
      "dispatch",
    ]);
    const steerItem = state.items[0];
    if (steerItem.kind === "user") {
      expect(steerItem.steered).toBe(true);
      expect(steerItem.text).toBe("switch to rust");
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
    // On a sequence gap, lastSeq MUST stay at the last contiguous sequence number
    // so a Subscribe { last_seq: 1 } resync recovers events 2 and 3 without loss.
    expect(state.lastSeq).toBe(1);
    expect(state.gapDetectedCount).toBe(1);

    state = applySessionEvents(state, [
      ev(2, {
        type: "agent_message_chunk",
        turn_id: "t1",
        content: { type: "text", text: "," },
      }),
      ev(3, {
        type: "agent_message_chunk",
        turn_id: "t1",
        content: { type: "text", text: " " },
      }),
      ev(4, {
        type: "agent_message_chunk",
        turn_id: "t1",
        content: { type: "text", text: "world" },
      }),
    ]);
    expect(state.lastSeq).toBe(4);
    const assistant = state.items[0];
    if (assistant.kind === "assistant") {
      expect(assistant.text).toBe("hello, world");
    }
  });

  it("preserves tool_call streamed output on raw_output update and clears queuePaused when emptied", () => {
    let state = createEmptyProjection("s1", "running");
    state = applySessionEvents(state, [
      ev(1, {
        type: "tool_call",
        turn_id: "t1",
        tool_call: {
          tool_call_id: "c1",
          title: "Run cargo test",
          kind: "execute",
          status: "in_progress",
        },
      }),
      ev(2, {
        type: "tool_call_update",
        turn_id: "t1",
        update: {
          tool_call_id: "c1",
          content: [{ type: "text", text: "stream line 1" }],
        },
      }),
      ev(3, {
        type: "tool_call_update",
        turn_id: "t1",
        update: {
          tool_call_id: "c1",
          status: "completed",
          raw_output: { exit_code: 0 },
        },
      }),
      ev(4, {
        type: "prompt_queued",
        position: 0,
        queued: {
          id: "q1",
          member_id: "alice",
          prompt: { content: [{ type: "text", text: "next" }] },
          mode: "queue",
          created_at: TS,
        },
      }),
      ev(5, { type: "queue_paused", paused: true }),
      ev(6, {
        type: "queue_item_removed",
        queued_id: "q1",
        reason: { reason: "withdrawn" },
      }),
    ]);

    expect(state.queue).toHaveLength(0);
    expect(state.queuePaused).toBe(false);
    const assistant = state.items[0];
    if (assistant.kind === "assistant") {
      expect(assistant.toolCalls[0].outputText).toContain("stream line 1");
      expect(assistant.toolCalls[0].outputText).toContain("exit_code");
    }
  });

  it("validates http/https URLs and derives collision-free profile IDs", async () => {
    expect(isSafeHttpUrl("https://om.internal:4939/takeover/1")).toBe(true);
    expect(isSafeHttpUrl("http://127.0.0.1:4939/takeover/1")).toBe(true);
    expect(isSafeHttpUrl("javascript:alert(1)")).toBe(false);
    expect(isSafeHttpUrl("data:text/html,<script>alert(1)</script>")).toBe(false);

    expect(normalizeServerUrl("https://om.internal:4939///")).toBe(
      "https://om.internal:4939"
    );
    expect(() => normalizeServerUrl("javascript:alert(1)")).toThrow();

    const idA = await computeProfileId("https://very-long-internal-host-a.corp.example.com:4939/env/alpha");
    const idB = await computeProfileId("https://very-long-internal-host-a.corp.example.com:4939/env/beta");
    expect(idA).not.toBe(idB);
    expect(idA).toMatch(/^srv_[a-z0-9_]+$/);
  });

  it("caps reconnect backoff", () => {
    expect(computeBackoffMs(1)).toBe(600);
    expect(computeBackoffMs(2)).toBeGreaterThan(600);
    expect(computeBackoffMs(20)).toBe(12_000);
  });
});
