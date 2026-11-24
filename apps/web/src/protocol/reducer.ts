import {
  renderContentBlockText,
  renderContentBlocks,
  type ApprovalRecord,
  type Dispatch,
  type Goal,
  type HarnessCapabilities,
  type PlanEntry,
  type QueuedPrompt,
  type SessionEvent,
  type SessionStatus,
  type StopReason,
  type ToolCallStatus,
  type ToolKind,
} from "./types";

export interface ProjectedToolCall {
  tool_call_id: string;
  title: string;
  kind: ToolKind;
  status: ToolCallStatus;
  rawInput: string;
  locations: string[];
  outputText: string;
  startedAt: number;
  updatedAt: number;
}

export type TimelineItem =
  | {
      kind: "user";
      id: string;
      turn_id: string;
      author: string;
      text: string;
      steered?: boolean;
      ts: number;
      seq: number;
    }
  | {
      kind: "assistant";
      id: string;
      turn_id: string;
      thinking: string;
      text: string;
      plan: PlanEntry[];
      toolCalls: ProjectedToolCall[];
      approvals: ApprovalRecord[];
      completed: boolean;
      stopReason?: StopReason;
      turnError?: string;
      ts: number;
      seq: number;
    }
  | {
      kind: "goal";
      id: string;
      goal: Goal;
      ts: number;
      seq: number;
    }
  | {
      kind: "dispatch";
      id: string;
      direction: "sent" | "received";
      dispatch: Dispatch;
      ts: number;
      seq: number;
    }
  | {
      kind: "error";
      id: string;
      message: string;
      ts: number;
      seq: number;
    };

export interface SessionProjection {
  sessionId: string;
  status: SessionStatus;
  capabilities: HarnessCapabilities | null;
  nativeSessionId: string | null;
  lastSeq: number;
  dedupedCount: number;
  gapDetectedCount: number;
  queue: QueuedPrompt[];
  queuePaused: boolean;
  goal: Goal | null;
  items: TimelineItem[];
  rawEvents: SessionEvent[];
}

const MAX_RAW_INSPECTOR_EVENTS = 250;
const MAX_TIMELINE_ITEMS = 1000;

export function createEmptyProjection(
  sessionId: string,
  initialStatus: SessionStatus = "idle"
): SessionProjection {
  return {
    sessionId,
    status: initialStatus,
    capabilities: null,
    nativeSessionId: null,
    lastSeq: 0,
    dedupedCount: 0,
    gapDetectedCount: 0,
    queue: [],
    queuePaused: false,
    goal: null,
    items: [],
    rawEvents: [],
  };
}

function formatJsonValue(value: unknown): string {
  if (value === undefined || value === null) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function findOrCreateAssistantIndex(
  items: TimelineItem[],
  turnId: string,
  ts: number,
  seq: number
): number {
  for (let i = items.length - 1; i >= 0; i -= 1) {
    const item = items[i];
    if (item.kind === "assistant" && item.turn_id === turnId) {
      return i;
    }
  }
  items.push({
    kind: "assistant",
    id: `assistant:${turnId}`,
    turn_id: turnId,
    thinking: "",
    text: "",
    plan: [],
    toolCalls: [],
    approvals: [],
    completed: false,
    ts,
    seq,
  });
  return items.length - 1;
}

/**
 * Pure, exhaustive, idempotent projector for `om-core::SessionEvent`.
 * - Deduplicates any `seq <= state.lastSeq` (safe across REST catchup + WS overlap).
 * - Flags sequence gaps (`seq > state.lastSeq + 1`) so callers can trigger resync.
 * - Treats `status_changed` as the sole authoritative source for `SessionStatus`
 *   (matching `om-server::orchestrator::actor` and W1-3 auto-resolved approvals).
 */
export function applySessionEvent(
  state: SessionProjection,
  record: SessionEvent
): SessionProjection {
  if (record.session_id !== state.sessionId) {
    return state;
  }

  if (record.seq <= state.lastSeq) {
    return {
      ...state,
      dedupedCount: state.dedupedCount + 1,
    };
  }

  const hasGap = state.lastSeq > 0 && record.seq > state.lastSeq + 1;
  if (hasGap) {
    // Hold `lastSeq` at the last contiguous sequence number so the caller triggers a
    // `Subscribe { last_seq }` resync instead of silently skipping missing events.
    return {
      ...state,
      gapDetectedCount: state.gapDetectedCount + 1,
    };
  }
  const items = state.items.slice();
  let status = state.status;
  let capabilities = state.capabilities;
  let nativeSessionId = state.nativeSessionId;
  let queue = state.queue;
  let queuePaused = state.queuePaused;
  let goal = state.goal;

  const ev = record.payload;
  switch (ev.type) {
    case "session_created": {
      break;
    }
    case "harness_ready": {
      capabilities = ev.capabilities;
      if (ev.native_session_id) {
        nativeSessionId = ev.native_session_id;
      }
      break;
    }
    case "status_changed": {
      status = ev.to;
      break;
    }
    case "prompt_submitted": {
      items.push({
        kind: "user",
        id: `user:${ev.turn_id}:${record.seq}`,
        turn_id: ev.turn_id,
        author: ev.member_id ?? "member",
        text: renderContentBlocks(ev.prompt.content),
        ts: record.ts,
        seq: record.seq,
      });
      break;
    }
    case "prompt_steered": {
      items.push({
        kind: "user",
        id: `steer:${ev.turn_id}:${record.seq}`,
        turn_id: ev.turn_id,
        author: ev.member_id ?? "member",
        text: renderContentBlocks(ev.prompt.content),
        steered: true,
        ts: record.ts,
        seq: record.seq,
      });
      break;
    }
    case "agent_thought_chunk": {
      const idx = findOrCreateAssistantIndex(
        items,
        ev.turn_id,
        record.ts,
        record.seq
      );
      const current = items[idx];
      if (current.kind === "assistant") {
        items[idx] = {
          ...current,
          thinking: current.thinking + renderContentBlockText(ev.content),
        };
      }
      break;
    }
    case "agent_message_chunk": {
      const idx = findOrCreateAssistantIndex(
        items,
        ev.turn_id,
        record.ts,
        record.seq
      );
      const current = items[idx];
      if (current.kind === "assistant") {
        items[idx] = {
          ...current,
          text: current.text + renderContentBlockText(ev.content),
        };
      }
      break;
    }
    case "tool_call": {
      const idx = findOrCreateAssistantIndex(
        items,
        ev.turn_id,
        record.ts,
        record.seq
      );
      const current = items[idx];
      if (current.kind === "assistant") {
        const tc = ev.tool_call;
        const existingIdx = current.toolCalls.findIndex(
          (t) => t.tool_call_id === tc.tool_call_id
        );
        const toolCalls =
          existingIdx >= 0
            ? current.toolCalls.map((t, i) =>
                i === existingIdx
                  ? {
                      ...t,
                      title: tc.title || t.title,
                      kind: tc.kind,
                      status: tc.status,
                      rawInput: formatJsonValue(tc.raw_input) || t.rawInput,
                      locations:
                        tc.locations && tc.locations.length > 0
                          ? tc.locations
                          : t.locations,
                      updatedAt: record.ts,
                    }
                  : t
              )
            : [
                ...current.toolCalls,
                {
                  tool_call_id: tc.tool_call_id,
                  title: tc.title,
                  kind: tc.kind,
                  status: tc.status,
                  rawInput: formatJsonValue(tc.raw_input),
                  locations: tc.locations ?? [],
                  outputText: "",
                  startedAt: record.ts,
                  updatedAt: record.ts,
                },
              ];
        items[idx] = { ...current, toolCalls };
      }
      break;
    }
    case "tool_call_update": {
      const idx = findOrCreateAssistantIndex(
        items,
        ev.turn_id,
        record.ts,
        record.seq
      );
      const current = items[idx];
      if (current.kind === "assistant") {
        const upd = ev.update;
        const chunkText = upd.content ? renderContentBlocks(upd.content) : "";
        const rawOut = formatJsonValue(upd.raw_output);
        const toolCalls = current.toolCalls.map((t) => {
          if (t.tool_call_id !== upd.tool_call_id) return t;
          let nextOutput = t.outputText;
          if (chunkText) {
            nextOutput = nextOutput ? `${nextOutput}\n${chunkText}` : chunkText;
          } else if (rawOut) {
            nextOutput =
              nextOutput && nextOutput !== rawOut
                ? `${nextOutput}\n${rawOut}`
                : rawOut;
          }
          return {
            ...t,
            title: upd.title ?? t.title,
            status: upd.status ?? t.status,
            outputText: nextOutput,
            updatedAt: record.ts,
          };
        });
        items[idx] = { ...current, toolCalls };
      }
      break;
    }
    case "plan": {
      const idx = findOrCreateAssistantIndex(
        items,
        ev.turn_id,
        record.ts,
        record.seq
      );
      const current = items[idx];
      if (current.kind === "assistant") {
        items[idx] = {
          ...current,
          plan: ev.entries,
        };
      }
      break;
    }
    case "approval_requested": {
      const syntheticTurn = !ev.turn_id;
      const turnId = ev.turn_id ?? `approval:${ev.approval.id}`;
      const idx = findOrCreateAssistantIndex(items, turnId, record.ts, record.seq);
      const current = items[idx];
      if (current.kind === "assistant") {
        const exists = current.approvals.some((a) => a.id === ev.approval.id);
        const approvals = exists
          ? current.approvals
          : [...current.approvals, ev.approval];
        items[idx] = {
          ...current,
          approvals,
          completed: syntheticTurn ? true : current.completed,
        };
      }
      break;
    }
    case "approval_resolved": {
      for (let i = items.length - 1; i >= 0; i -= 1) {
        const item = items[i];
        if (item.kind !== "assistant") continue;
        if (!item.approvals.some((a) => a.id === ev.approval_id)) continue;
        const approvals: ApprovalRecord[] = item.approvals.map((a) =>
          a.id === ev.approval_id
            ? {
                ...a,
                status:
                  ev.decision.outcome === "cancelled"
                    ? "cancelled"
                    : "resolved",
                decision: ev.decision,
                resolved_by: ev.resolved_by,
                auto_rule: ev.auto_rule,
                resolved_at: record.ts,
              }
            : a
        );
        items[i] = { ...item, approvals };
        break;
      }
      break;
    }
    case "turn_ended": {
      const idx = findOrCreateAssistantIndex(
        items,
        ev.turn_id,
        record.ts,
        record.seq
      );
      const current = items[idx];
      if (current.kind === "assistant") {
        items[idx] = {
          ...current,
          completed: true,
          stopReason: ev.stop_reason,
          turnError: ev.error,
        };
      }
      break;
    }
    case "error": {
      items.push({
        kind: "error",
        id: `error:${record.seq}`,
        message: ev.message,
        ts: record.ts,
        seq: record.seq,
      });
      break;
    }
    case "prompt_queued": {
      const next = queue.filter((q) => q.id !== ev.queued.id);
      const pos = Math.max(0, Math.min(ev.position, next.length));
      next.splice(pos, 0, ev.queued);
      queue = next;
      break;
    }
    case "queue_item_removed": {
      queue = queue.filter((q) => q.id !== ev.queued_id);
      if (queue.length === 0) {
        queuePaused = false;
      }
      break;
    }
    case "queue_reordered": {
      const byId = new Map(queue.map((q) => [q.id, q]));
      const ordered: QueuedPrompt[] = [];
      for (const id of ev.order) {
        const found = byId.get(id);
        if (found) {
          ordered.push(found);
          byId.delete(id);
        }
      }
      for (const leftover of byId.values()) {
        ordered.push(leftover);
      }
      queue = ordered;
      break;
    }
    case "queue_paused": {
      queuePaused = ev.paused;
      break;
    }
    case "goal_updated": {
      goal = ev.goal;
      items.push({
        kind: "goal",
        id: `goal:${ev.goal.id}:${record.seq}`,
        goal: ev.goal,
        ts: record.ts,
        seq: record.seq,
      });
      break;
    }
    case "dispatch_sent": {
      items.push({
        kind: "dispatch",
        id: `dispatch_sent:${ev.dispatch.id}:${record.seq}`,
        direction: "sent",
        dispatch: ev.dispatch,
        ts: record.ts,
        seq: record.seq,
      });
      break;
    }
    case "dispatch_received": {
      items.push({
        kind: "dispatch",
        id: `dispatch_received:${ev.dispatch.id}:${record.seq}`,
        direction: "received",
        dispatch: ev.dispatch,
        ts: record.ts,
        seq: record.seq,
      });
      break;
    }
    default: {
      const _exhaustive: never = ev;
      void _exhaustive;
    }
  }

  const boundedItems =
    items.length > MAX_TIMELINE_ITEMS
      ? items.slice(items.length - MAX_TIMELINE_ITEMS)
      : items;

  const nextRaw =
    state.rawEvents.length >= MAX_RAW_INSPECTOR_EVENTS
      ? [...state.rawEvents.slice(-(MAX_RAW_INSPECTOR_EVENTS - 1)), record]
      : [...state.rawEvents, record];

  return {
    sessionId: state.sessionId,
    status,
    capabilities,
    nativeSessionId,
    lastSeq: record.seq,
    dedupedCount: state.dedupedCount,
    gapDetectedCount: state.gapDetectedCount,
    queue,
    queuePaused,
    goal,
    items: boundedItems,
    rawEvents: nextRaw,
  };
}

export function applySessionEvents(
  state: SessionProjection,
  records: readonly SessionEvent[]
): SessionProjection {
  const ordered = [...records].sort((a, b) => a.seq - b.seq);
  return ordered.reduce(applySessionEvent, state);
}
