import {
  renderContentBlockText,
  renderContentBlocks,
  type ApprovalRecord,
  type HarnessCapabilities,
  type PlanEntry,
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
  items: TimelineItem[];
  rawEvents: SessionEvent[];
}

const MAX_RAW_INSPECTOR_EVENTS = 250;

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
 * Pure, idempotent projector for `om-core::SessionEvent`.
 * - Deduplicates any `seq <= state.lastSeq` (safe across REST catchup + WS overlap).
 * - Flags sequence gaps (`seq > state.lastSeq + 1`) so callers can trigger resync.
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
  const items = state.items.slice();
  let status = state.status;
  let capabilities = state.capabilities;
  let nativeSessionId = state.nativeSessionId;

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
        const exists = current.toolCalls.some(
          (t) => t.tool_call_id === tc.tool_call_id
        );
        const toolCalls = exists
          ? current.toolCalls
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
          const nextOutput = chunkText
            ? t.outputText
              ? `${t.outputText}\n${chunkText}`
              : chunkText
            : rawOut || t.outputText;
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
      const turnId = ev.turn_id ?? `approval:${ev.approval.id}`;
      const idx = findOrCreateAssistantIndex(items, turnId, record.ts, record.seq);
      const current = items[idx];
      if (current.kind === "assistant") {
        const exists = current.approvals.some((a) => a.id === ev.approval.id);
        const approvals = exists
          ? current.approvals
          : [...current.approvals, ev.approval];
        items[idx] = { ...current, approvals };
      }
      status = "waiting_approval";
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
                resolved_at: record.ts,
              }
            : a
        );
        items[i] = { ...item, approvals };
        break;
      }
      if (status === "waiting_approval") {
        status = "running";
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
      if (ev.stop_reason === "failed") {
        status = "failed";
      } else if (status !== "closed") {
        status = "idle";
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
      status = "failed";
      break;
    }
  }

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
    gapDetectedCount: state.gapDetectedCount + (hasGap ? 1 : 0),
    items,
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
