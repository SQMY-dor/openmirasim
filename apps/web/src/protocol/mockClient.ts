import type {
  EventSubscription,
  EventSubscriptionHandlers,
  OpenMirasimClient,
} from "./client";
import {
  KNOWN_HARNESSES,
  type ApprovalDecision,
  type ApprovalId,
  type ApprovalRecord,
  type CreateSessionRequest,
  type EventPayload,
  type ServerInfo,
  type SessionEvent,
  type SessionId,
  type SessionSummary,
  type SubmitPromptResponse,
} from "./types";

const STORAGE_KEY = "openmirasim.mock.store.v2";

interface PersistedMockStore {
  sessions: SessionSummary[];
  eventsBySession: Record<string, SessionEvent[]>;
}

interface PendingApprovalGate {
  sessionId: SessionId;
  approvalId: ApprovalId;
  turnId: string;
  resolve: (decision: ApprovalDecision) => void;
}

interface ActiveRun {
  cancelled: boolean;
  approvalGate?: PendingApprovalGate;
}

interface ListenerEntry {
  sessionId: SessionId;
  getLastSeq: () => number;
  handlers: EventSubscriptionHandlers;
  paused: boolean;
}

function nowMs(): number {
  return Date.now();
}

function makeId(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 7)}`;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

function defaultSeedStore(): PersistedMockStore {
  const ts = nowMs();
  const sessionId = "sess_demo_opencode";
  const opencode = KNOWN_HARNESSES[0];

  const config = {
    harness_id: "opencode",
    cwd: "/srv/workspaces/openmirasim",
    sandbox: "direct" as const,
    title: "Phase 0 架构核对（演示会话）",
  };

  const events: SessionEvent[] = [
    {
      session_id: sessionId,
      seq: 1,
      ts,
      payload: {
        type: "session_created",
        config,
        created_by: "member_dev",
      },
    },
    {
      session_id: sessionId,
      seq: 2,
      ts,
      payload: {
        type: "harness_ready",
        capabilities: opencode.capabilities,
        native_session_id: "acp_demo_001",
      },
    },
    {
      session_id: sessionId,
      seq: 3,
      ts,
      payload: {
        type: "prompt_submitted",
        turn_id: "turn_seed_1",
        member_id: "member_dev",
        prompt: {
          content: [
            {
              type: "text",
              text: "检查当前仓库的 Phase 0 契约，并确认 `last_seq` 断线续传协议。",
            },
          ],
        },
      },
    },
    {
      session_id: sessionId,
      seq: 4,
      ts,
      payload: {
        type: "plan",
        turn_id: "turn_seed_1",
        entries: [
          { content: "读取 om-core ts-rs 生成绑定", status: "completed" },
          { content: "核对 /ws/events 断线续传握手", status: "completed" },
        ],
      },
    },
    {
      session_id: sessionId,
      seq: 5,
      ts,
      payload: {
        type: "tool_call",
        turn_id: "turn_seed_1",
        tool_call: {
          tool_call_id: "call_seed_read",
          title: "Read crates/om-core/src/api.rs",
          kind: "read",
          status: "in_progress",
          raw_input: { path: "crates/om-core/src/api.rs" },
          locations: ["crates/om-core/src/api.rs"],
        },
      },
    },
    {
      session_id: sessionId,
      seq: 6,
      ts,
      payload: {
        type: "tool_call_update",
        turn_id: "turn_seed_1",
        update: {
          tool_call_id: "call_seed_read",
          status: "completed",
          content: [
            {
              type: "text",
              text: "Verified Hello -> Welcome -> Subscribe { last_seq } -> Subscribed { head_seq } -> Event / Resync",
            },
          ],
        },
      },
    },
    {
      session_id: sessionId,
      seq: 7,
      ts,
      payload: {
        type: "agent_message_chunk",
        turn_id: "turn_seed_1",
        content: {
          type: "text",
          text: "已直接绑定 `crates/om-core` 生成的 `ts-rs` 契约：\n\n- **协议握手**：`/ws/events` 采用 `Hello -> Welcome -> Subscribe { last_seq } -> Subscribed { head_seq }`，慢客户端触发 `Resync` 自动重订阅。\n- **验证提示**：输入任意消息体验流式回复、Plan 与 ToolCall 更新；输入包含 **「审批」** 或 **`deploy`** 的指令触发 ACP 审批卡片；点击右上角 **「模拟断网」** 验证 `last_seq` 无缝补齐。",
        },
      },
    },
    {
      session_id: sessionId,
      seq: 8,
      ts,
      payload: {
        type: "turn_ended",
        turn_id: "turn_seed_1",
        stop_reason: "end_turn",
      },
    },
  ];

  return {
    sessions: [
      {
        id: sessionId,
        space_id: "space_default",
        config,
        status: "idle",
        head_seq: 8,
        native_session_id: "acp_demo_001",
        created_at: ts,
        updated_at: ts,
      },
    ],
    eventsBySession: {
      [sessionId]: events,
    },
  };
}

/**
 * Local in-browser mock transport strictly typed against `crates/om-core` ts-rs bindings.
 */
export class MockOpenMirasimClient implements OpenMirasimClient {
  readonly mode = "mock" as const;
  readonly serverUrl = "mock://local-preview";

  private store: PersistedMockStore;
  private listeners = new Set<ListenerEntry>();
  private activeRuns = new Map<string, ActiveRun>();
  private approvalLookup = new Map<ApprovalId, PendingApprovalGate>();

  constructor() {
    this.store = this.loadStore();
  }

  private loadStore(): PersistedMockStore {
    try {
      const raw = window.localStorage.getItem(STORAGE_KEY);
      if (raw) {
        const parsed = JSON.parse(raw) as PersistedMockStore;
        if (parsed && Array.isArray(parsed.sessions) && parsed.eventsBySession) {
          return parsed;
        }
      }
    } catch {
      // fallback to seed
    }
    const seed = defaultSeedStore();
    this.saveStore(seed);
    return seed;
  }

  private saveStore(store: PersistedMockStore = this.store): void {
    try {
      window.localStorage.setItem(STORAGE_KEY, JSON.stringify(store));
    } catch {
      // ignore storage quota errors
    }
  }

  private appendEvent(
    sessionId: SessionId,
    payload: EventPayload
  ): SessionEvent {
    const existing = this.store.eventsBySession[sessionId] ?? [];
    const nextSeq = existing.length > 0 ? existing[existing.length - 1].seq + 1 : 1;
    const record: SessionEvent = {
      session_id: sessionId,
      seq: nextSeq,
      ts: nowMs(),
      payload,
    };

    this.store.eventsBySession[sessionId] = [...existing, record];
    this.store.sessions = this.store.sessions.map((s) => {
      if (s.id !== sessionId) return s;
      let status = s.status;
      if (payload.type === "status_changed") status = payload.to;
      if (payload.type === "approval_requested") status = "waiting_approval";
      if (payload.type === "approval_resolved") status = "running";
      if (payload.type === "turn_ended") {
        status = payload.stop_reason === "failed" ? "failed" : "idle";
      }
      return {
        ...s,
        status,
        head_seq: nextSeq,
        updated_at: record.ts,
      };
    });
    this.saveStore();

    for (const entry of this.listeners) {
      if (entry.sessionId === sessionId && !entry.paused) {
        entry.handlers.onEvent(record);
      }
    }

    return record;
  }

  async connect(): Promise<ServerInfo> {
    await sleep(50);
    return {
      serverVersion: "0.1.0-mock",
      spaceLabel: "OpenMirasim Team Space (Local Mock)",
    };
  }

  async listSessions(): Promise<SessionSummary[]> {
    this.store = this.loadStore();
    return [...this.store.sessions].sort((a, b) => b.updated_at - a.updated_at);
  }

  async createSession(req: CreateSessionRequest): Promise<SessionSummary> {
    const info =
      KNOWN_HARNESSES.find((h) => h.id === req.config.harness_id) ??
      KNOWN_HARNESSES[0];
    const ts = nowMs();
    const sessionId = makeId("sess");
    const config = {
      ...req.config,
      title:
        req.config.title?.trim() ||
        `${info.id} · ${req.config.cwd.split("/").pop() || "workspace"}`,
    };

    const session: SessionSummary = {
      id: sessionId,
      space_id: "space_default",
      config,
      status: "idle",
      head_seq: 0,
      created_at: ts,
      updated_at: ts,
    };

    this.store.sessions = [session, ...this.store.sessions];
    this.store.eventsBySession[sessionId] = [];
    this.saveStore();

    this.appendEvent(sessionId, {
      type: "session_created",
      config,
      created_by: "member_dev",
    });
    this.appendEvent(sessionId, {
      type: "harness_ready",
      capabilities: info.capabilities,
      native_session_id: makeId("acp"),
    });

    return (
      this.store.sessions.find((s) => s.id === sessionId) ?? session
    );
  }

  async fetchEvents(
    sessionId: SessionId,
    afterSeq: number
  ): Promise<SessionEvent[]> {
    this.store = this.loadStore();
    const all = this.store.eventsBySession[sessionId] ?? [];
    return all.filter((e) => e.seq > afterSeq);
  }

  async submitPrompt(
    sessionId: SessionId,
    text: string
  ): Promise<SubmitPromptResponse> {
    const trimmed = text.trim();
    const turnId = makeId("turn");
    if (!trimmed) return { turn_id: turnId };

    const run: ActiveRun = { cancelled: false };
    this.activeRuns.set(sessionId, run);

    this.appendEvent(sessionId, {
      type: "status_changed",
      from: "idle",
      to: "running",
    });
    this.appendEvent(sessionId, {
      type: "prompt_submitted",
      turn_id: turnId,
      member_id: "member_dev",
      prompt: { content: [{ type: "text", text: trimmed }] },
    });

    void this.runMockTurn(sessionId, turnId, trimmed, run);
    return { turn_id: turnId };
  }

  private async runMockTurn(
    sessionId: SessionId,
    turnId: string,
    prompt: string,
    run: ActiveRun
  ): Promise<void> {
    const needsApproval = /审批|approve|deploy|rm\s+-rf|sudo|发布/i.test(prompt);
    const callId = makeId("call");

    this.appendEvent(sessionId, {
      type: "plan",
      turn_id: turnId,
      entries: [
        { content: "解析指令并定位受影响文件", status: "in_progress" },
        {
          content: needsApproval ? "申请高危命令审批并执行" : "整理结构化分析结果",
          status: "pending",
        },
      ],
    });

    for (const chunk of [
      "分析工作区上下文…",
      needsApproval
        ? " 检测到越界命令，准备通过 ACP `session/request_permission` 发起审批。"
        : " 准备执行只读检索并流式输出结论。",
    ]) {
      await sleep(70);
      if (run.cancelled) return this.finishCancelled(sessionId, turnId);
      this.appendEvent(sessionId, {
        type: "agent_thought_chunk",
        turn_id: turnId,
        content: { type: "text", text: chunk },
      });
    }

    await sleep(80);
    if (run.cancelled) return this.finishCancelled(sessionId, turnId);

    this.appendEvent(sessionId, {
      type: "tool_call",
      turn_id: turnId,
      tool_call: {
        tool_call_id: callId,
        title: needsApproval
          ? "Execute systemctl reload openmirasim-server"
          : "Search crates/ workspace",
        kind: needsApproval ? "execute" : "search",
        status: "in_progress",
        raw_input: needsApproval
          ? { cmd: "systemctl reload openmirasim-server" }
          : { pattern: prompt.slice(0, 40), path: "crates/" },
        locations: needsApproval ? ["/etc/systemd/system"] : ["crates/om-core"],
      },
    });

    if (needsApproval) {
      await sleep(75);
      if (run.cancelled) return this.finishCancelled(sessionId, turnId);

      const approvalId = makeId("appr");
      const approvalRecord: ApprovalRecord = {
        id: approvalId,
        session_id: sessionId,
        tool_call_id: callId,
        title: "执行宿主机命令 `systemctl reload openmirasim-server`",
        options: [
          { option_id: "allow_once", name: "允许一次", kind: "allow_once" },
          { option_id: "allow_always", name: "始终允许", kind: "allow_always" },
          { option_id: "reject_once", name: "拒绝", kind: "reject_once" },
        ],
        status: "pending",
        created_at: nowMs(),
      };

      this.appendEvent(sessionId, {
        type: "approval_requested",
        turn_id: turnId,
        approval: approvalRecord,
      });

      const decision = await new Promise<ApprovalDecision>((resolve) => {
        const gate: PendingApprovalGate = {
          sessionId,
          approvalId,
          turnId,
          resolve,
        };
        run.approvalGate = gate;
        this.approvalLookup.set(approvalId, gate);
      });
      this.approvalLookup.delete(approvalId);
      run.approvalGate = undefined;
      if (run.cancelled) return this.finishCancelled(sessionId, turnId);

      const allowed =
        decision.outcome === "selected" &&
        (decision.option_id === "allow_once" ||
          decision.option_id === "allow_always");

      await sleep(80);
      this.appendEvent(sessionId, {
        type: "tool_call_update",
        turn_id: turnId,
        update: {
          tool_call_id: callId,
          status: allowed ? "completed" : "failed",
          content: [
            {
              type: "text",
              text: allowed
                ? "Reloaded openmirasim-server.service (exit 0)"
                : "Denied by member approval decision.",
            },
          ],
        },
      });

      this.appendEvent(sessionId, {
        type: "plan",
        turn_id: turnId,
        entries: [
          { content: "解析指令并定位受影响文件", status: "completed" },
          {
            content: "申请高危命令审批并执行",
            status: "completed",
          },
        ],
      });

      const chunks = allowed
        ? [
            "审批已通过，受控命令执行完成。\n\n",
            "```bash\n$ systemctl reload openmirasim-server\nReloaded openmirasim-server.service (exit 0)\n```\n",
            "本次审批结果已按单调 `seq` 持久化记录。",
          ]
        : [
            "已收到拒绝审批决策，未执行高危命令。\n\n",
            "如需改用只读 dry-run 检查，请直接发送下一条指令。",
          ];

      for (const delta of chunks) {
        await sleep(70);
        if (run.cancelled) return this.finishCancelled(sessionId, turnId);
        this.appendEvent(sessionId, {
          type: "agent_message_chunk",
          turn_id: turnId,
          content: { type: "text", text: delta },
        });
      }
    } else {
      await sleep(120);
      if (run.cancelled) return this.finishCancelled(sessionId, turnId);
      this.appendEvent(sessionId, {
        type: "tool_call_update",
        turn_id: turnId,
        update: {
          tool_call_id: callId,
          status: "completed",
          content: [
            {
              type: "text",
              text: "Matched 3 files in crates/om-core, crates/om-server, apps/web",
            },
          ],
        },
      });

      this.appendEvent(sessionId, {
        type: "plan",
        turn_id: turnId,
        entries: [
          { content: "解析指令并定位受影响文件", status: "completed" },
          { content: "整理结构化分析结果", status: "completed" },
        ],
      });

      const chunks = [
        `已收到指令：「${prompt}」\n\n`,
        "当前正在 **Local Mock 演示模式** 下模拟 `om-server` 与 `om-core` 的真实事件流：\n\n",
        "1. 每个 `agent_message_chunk`、`plan` 与 `tool_call_update` 均带有严格递增 `seq`。\n",
        "2. 刷新页面或点击 **「模拟断网」** 后，客户端会发送 `Subscribe { last_seq }` 自动补齐。\n\n",
        "```json\n{\"type\":\"subscribe\",\"session_id\":\"...\",\"last_seq\":12}\n```",
      ];

      for (const delta of chunks) {
        await sleep(70);
        if (run.cancelled) return this.finishCancelled(sessionId, turnId);
        this.appendEvent(sessionId, {
          type: "agent_message_chunk",
          turn_id: turnId,
          content: { type: "text", text: delta },
        });
      }
    }

    if (run.cancelled) return this.finishCancelled(sessionId, turnId);
    this.appendEvent(sessionId, {
      type: "turn_ended",
      turn_id: turnId,
      stop_reason: "end_turn",
    });
    this.activeRuns.delete(sessionId);
  }

  private finishCancelled(sessionId: SessionId, turnId: string): void {
    this.appendEvent(sessionId, {
      type: "turn_ended",
      turn_id: turnId,
      stop_reason: "cancelled",
    });
    this.activeRuns.delete(sessionId);
  }

  async cancelSession(sessionId: SessionId): Promise<void> {
    const run = this.activeRuns.get(sessionId);
    if (!run) return;
    run.cancelled = true;
    if (run.approvalGate) {
      run.approvalGate.resolve({ outcome: "cancelled" });
    }
  }

  async resolveApproval(
    approvalId: ApprovalId,
    decision: ApprovalDecision
  ): Promise<void> {
    const gate = this.approvalLookup.get(approvalId);
    if (!gate) return;
    this.appendEvent(gate.sessionId, {
      type: "approval_resolved",
      approval_id: approvalId,
      decision,
      resolved_by: "member_dev",
    });
    gate.resolve(decision);
  }

  subscribeSession(
    sessionId: SessionId,
    getLastSeq: () => number,
    handlers: EventSubscriptionHandlers
  ): EventSubscription {
    const entry: ListenerEntry = {
      sessionId,
      getLastSeq,
      handlers,
      paused: false,
    };
    this.listeners.add(entry);

    handlers.onStateChange("catching_up", null);
    void this.fetchEvents(sessionId, getLastSeq()).then((missed) => {
      if (!this.listeners.has(entry)) return;
      const headSeq =
        this.store.sessions.find((s) => s.id === sessionId)?.head_seq ?? 0;
      handlers.onHeadSeq?.(headSeq);
      for (const ev of missed) {
        handlers.onEvent(ev);
      }
      handlers.onStateChange("connected", null);
    });

    return {
      close: () => {
        this.listeners.delete(entry);
        handlers.onStateChange("disconnected", null);
      },
    };
  }

  async simulateNetworkDrop(sessionId: SessionId): Promise<void> {
    const active = [...this.listeners].filter((l) => l.sessionId === sessionId);
    for (const l of active) {
      l.paused = true;
      l.handlers.onReconnectAttempt?.(1);
      l.handlers.onStateChange("reconnecting", "模拟网络中断中（后台继续产生事件）");
    }

    const turnId = makeId("turn_offline");
    await sleep(200);
    this.appendEvent(sessionId, {
      type: "prompt_submitted",
      turn_id: turnId,
      member_id: "system",
      prompt: {
        content: [
          {
            type: "text",
            text: "[离线期间服务端事件] 验证断线重连后按 last_seq 无缝补齐",
          },
        ],
      },
    });
    await sleep(140);
    this.appendEvent(sessionId, {
      type: "agent_message_chunk",
      turn_id: turnId,
      content: {
        type: "text",
        text: "该条消息产生于客户端断网期间，重连后已通过 `last_seq` 完整追平，无重复无丢失。",
      },
    });
    await sleep(110);
    this.appendEvent(sessionId, {
      type: "turn_ended",
      turn_id: turnId,
      stop_reason: "end_turn",
    });

    await sleep(160);
    for (const l of active) {
      if (!this.listeners.has(l)) continue;
      l.handlers.onResync?.();
      l.handlers.onStateChange("catching_up", null);
      const missed = await this.fetchEvents(sessionId, l.getLastSeq());
      for (const ev of missed) {
        l.handlers.onEvent(ev);
      }
      const headSeq =
        this.store.sessions.find((s) => s.id === sessionId)?.head_seq ?? 0;
      l.handlers.onHeadSeq?.(headSeq);
      l.paused = false;
      l.handlers.onStateChange("connected", null);
    }
  }
}
