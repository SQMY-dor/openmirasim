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
  type ApprovalRuleView,
  type AuditEntry,
  type CreateMcpServerRequest,
  type CreateSessionRequest,
  type CreateSkillRequest,
  type DeviceTokenRecord,
  type Dispatch,
  type EffectiveHubResponse,
  type EventPayload,
  type Goal,
  type GoalId,
  type HarnessDescriptor,
  type HubScopeLevel,
  type IssuedToken,
  type McpProbeResult,
  type McpServerRecord,
  type MemberId,
  type MemberRecord,
  type NewDispatch,
  type NewGoal,
  type NewSchedule,
  type Principal,
  type ProviderInfo,
  type ProxyStats,
  type PushConfigResponse,
  type PushSubscriptionRecord,
  type PushTestResponse,
  type QueuedPromptId,
  type RegisterPushSubscriptionRequest,
  type RemoteScreenResponse,
  type Role,
  type Schedule,
  type ScheduleId,
  type ScheduleRun,
  type ServerInfo,
  type SessionEvent,
  type SessionId,
  type SessionSummary,
  type SkillRecord,
  type SubmitMode,
  type SubmitOutcome,
  type SubmitPromptResponse,
  type TokenId,
  type UpdateMcpServerRequest,
  type UpdateSkillRequest,
  type WorkspaceId,
  type WorkspaceSummary,
  type WorktreeDiff,
  type WorktreeEntry,
  type WorktreeFileContent,
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
  private mockMembers: MemberRecord[] = [
    {
      id: "member_dev",
      space_id: "space_default",
      display_name: "Demo Owner",
      role: "owner",
      created_at: nowMs() - 86_400_000,
    },
    {
      id: "member_viewer",
      space_id: "space_default",
      display_name: "Demo Viewer",
      role: "viewer",
      created_at: nowMs() - 43_200_000,
    },
  ];
  private mockTokens: DeviceTokenRecord[] = [
    {
      id: "tok_demo_owner",
      member_id: "member_dev",
      label: "Local Preview",
      created_at: nowMs() - 86_400_000,
    },
  ];
  private mockAudit: AuditEntry[] = [
    {
      id: 1,
      ts: nowMs() - 3_600_000,
      actor: "member_dev",
      action: "member.created",
      target: "member_viewer",
      detail: { role: "viewer" },
    },
  ];
  private mockPushSubs: PushSubscriptionRecord[] = [];

  constructor() {
    this.store = this.loadStore();
  }

  async getMe(): Promise<Principal> {
    return {
      member_id: "member_dev",
      space_id: "space_default",
      display_name: "Demo Owner",
      role: "owner",
      token_id: "tok_demo_owner",
    };
  }

  async listMembers(): Promise<MemberRecord[]> {
    return [...this.mockMembers];
  }

  async addMember(displayName: string, role: Role): Promise<MemberRecord> {
    const rec: MemberRecord = {
      id: makeId("mem"),
      space_id: "space_default",
      display_name: displayName,
      role,
      created_at: nowMs(),
    };
    this.mockMembers.push(rec);
    this.mockAudit.unshift({
      id: this.mockAudit.length + 1,
      ts: nowMs(),
      actor: "member_dev",
      action: "member.created",
      target: rec.id,
      detail: { role },
    });
    return rec;
  }

  async changeMemberRole(
    memberId: MemberId,
    role: Role
  ): Promise<MemberRecord> {
    const found = this.mockMembers.find((m) => m.id === memberId);
    if (!found) throw new Error(`member ${memberId} not found`);
    found.role = role;
    return { ...found };
  }

  async disableMember(memberId: MemberId): Promise<MemberRecord> {
    const found = this.mockMembers.find((m) => m.id === memberId);
    if (!found) throw new Error(`member ${memberId} not found`);
    found.disabled_at = nowMs();
    return { ...found };
  }

  async listMemberTokens(memberId: MemberId): Promise<DeviceTokenRecord[]> {
    return this.mockTokens.filter((t) => t.member_id === memberId);
  }

  async issueMemberToken(
    memberId: MemberId,
    label: string,
    ttlSecs?: number
  ): Promise<IssuedToken> {
    const now = nowMs();
    const token: DeviceTokenRecord = {
      id: makeId("tok"),
      member_id: memberId,
      label,
      created_at: now,
      ...(ttlSecs !== undefined ? { expires_at: now + ttlSecs * 1000 } : {}),
    };
    this.mockTokens.push(token);
    return {
      token,
      secret: `omt_${"a1b2c3d4".repeat(8)}`,
    };
  }

  async revokeToken(tokenId: TokenId): Promise<DeviceTokenRecord> {
    const found = this.mockTokens.find((t) => t.id === tokenId);
    if (!found) throw new Error(`token ${tokenId} not found`);
    found.revoked_at = nowMs();
    return { ...found };
  }

  async listAudit(target?: string, limit = 100): Promise<AuditEntry[]> {
    const filtered = target?.trim()
      ? this.mockAudit.filter((a) => a.target === target.trim())
      : this.mockAudit;
    return filtered.slice(0, limit);
  }

  async listProviders(): Promise<ProviderInfo[]> {
    return [
      { id: "anthropic", kind: "anthropic" },
      { id: "deepseek", kind: "open_ai" },
    ];
  }

  async getSessionProxyStats(): Promise<ProxyStats> {
    return { requests: 0, upstream_errors: 0 };
  }

  async getRemoteScreen(): Promise<RemoteScreenResponse> {
    return { available: false };
  }

  async getPushConfig(): Promise<PushConfigResponse> {
    return {
      enabled: true,
      subscription_count: this.mockPushSubs.length,
    };
  }

  async listPushSubscriptions(): Promise<PushSubscriptionRecord[]> {
    return [...this.mockPushSubs];
  }

  async registerPushSubscription(
    req: RegisterPushSubscriptionRequest
  ): Promise<PushSubscriptionRecord> {
    const rec: PushSubscriptionRecord = {
      id: makeId("push"),
      member_id: "member_dev",
      endpoint: req.endpoint,
      keys: req.keys,
      user_agent: req.user_agent,
      created_at: nowMs(),
    };
    this.mockPushSubs.push(rec);
    return rec;
  }

  async deletePushSubscription(subId: string): Promise<void> {
    this.mockPushSubs = this.mockPushSubs.filter((s) => s.id !== subId);
  }

  async testPush(): Promise<PushTestResponse> {
    return {
      delivered: this.mockPushSubs.length,
      mode: "service_worker_local",
      message: "本地演示模式：已就绪 Service Worker 审批通知",
    };
  }

  async getPtyStatus(): Promise<import("./types").PtyStatus> {
    return {
      active: true,
      cols: 100,
      rows: 30,
      has_recording: true,
      recording_bytes: 256,
      clients: 1,
    };
  }

  async killPty(): Promise<void> {
    // no-op in mock preview
  }

  async fetchPtyRecording(): Promise<string> {
    const header = JSON.stringify({
      version: 2,
      width: 100,
      height: 30,
      timestamp: Math.floor(Date.now() / 1000),
      env: { TERM: "xterm-256color", SHELL: "/bin/bash" },
    });
    const ev1 = JSON.stringify([0.05, "o", "openmirasim$ cargo test --workspace\r\n"]);
    const ev2 = JSON.stringify([0.35, "o", "\x1b[32mtest result: ok. all checks passed\x1b[0m\r\n"]);
    return `${header}\n${ev1}\n${ev2}\n`;
  }

  buildPtyWsUrl(): string | null {
    return null;
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
      const status =
        payload.type === "status_changed" ? payload.to : s.status;
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

  async listHarnesses(): Promise<HarnessDescriptor[]> {
    return KNOWN_HARNESSES.map((h) => ({
      id: h.id,
      capabilities: h.capabilities,
    }));
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
      status: "starting",
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
    this.appendEvent(sessionId, {
      type: "status_changed",
      from: "starting",
      to: "idle",
    });

    return (
      this.store.sessions.find((s) => s.id === sessionId) ?? session
    );
  }

  async closeSession(sessionId: SessionId): Promise<void> {
    const found = this.store.sessions.find((s) => s.id === sessionId);
    if (!found || found.status === "closed") return;
    await this.cancelSession(sessionId);
    this.appendEvent(sessionId, {
      type: "status_changed",
      from: found.status,
      to: "closed",
    });
  }

  async listWorkspaces(): Promise<WorkspaceSummary[]> {
    return [
      {
        id: "ws_demo_openmirasim",
        space_id: "space_default",
        name: "openmirasim",
        repo_path: "/srv/workspaces/openmirasim",
        default_branch: "main",
        created_at: nowMs(),
      },
    ];
  }

  async registerWorkspace(
    name: string,
    repoPath: string
  ): Promise<WorkspaceSummary> {
    return {
      id: makeId("ws"),
      space_id: "space_default",
      name,
      repo_path: repoPath,
      default_branch: "main",
      created_at: nowMs(),
    };
  }

  async getWorktreeDiff(_sessionId: SessionId): Promise<WorktreeDiff> {
    return {
      base_commit: "2a5344100000",
      files: [{ path: "README.md", kind: "modified" }],
      patch:
        "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1,2 +1,3 @@\n # OpenMirasim\n+Local Mock Worktree Diff Preview\n",
      truncated: false,
    };
  }

  async listWorktreeFiles(_sessionId: SessionId): Promise<WorktreeEntry[]> {
    return [
      { path: "README.md", size: 1024 },
      { path: "crates/om-core/src/api.rs", size: 6844 },
      { path: "apps/web/src/App.tsx", size: 17500 },
    ];
  }

  async readWorktreeFile(
    _sessionId: SessionId,
    path: string
  ): Promise<WorktreeFileContent> {
    return {
      path,
      content: `// Local Mock preview of ${path}\n`,
      utf8: true,
      size: 32 + path.length,
    };
  }

  async listApprovalRules(_sessionId: SessionId): Promise<ApprovalRuleView[]> {
    return [];
  }

  async clearApprovalRules(_sessionId: SessionId): Promise<number> {
    return 0;
  }

  async withdrawQueued(
    sessionId: SessionId,
    queuedId: QueuedPromptId
  ): Promise<void> {
    this.appendEvent(sessionId, {
      type: "queue_item_removed",
      queued_id: queuedId,
      reason: { reason: "withdrawn" },
    });
  }

  async moveQueued(
    _sessionId: SessionId,
    _queuedId: QueuedPromptId,
    _position: number
  ): Promise<void> {
    // no-op in mock preview
  }

  async resumeQueue(sessionId: SessionId): Promise<void> {
    this.appendEvent(sessionId, {
      type: "queue_paused",
      paused: false,
    });
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
    text: string,
    _mode?: SubmitMode
  ): Promise<SubmitPromptResponse | SubmitOutcome> {
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
        tool_kind: "execute",
        locations: ["/etc/systemd/system"],
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
      this.appendEvent(sessionId, {
        type: "status_changed",
        from: "running",
        to: "waiting_approval",
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
    this.appendEvent(sessionId, {
      type: "status_changed",
      from: "running",
      to: "idle",
    });
    this.activeRuns.delete(sessionId);
  }

  private finishCancelled(sessionId: SessionId, turnId: string): void {
    const current =
      this.store.sessions.find((s) => s.id === sessionId)?.status ?? "running";
    this.appendEvent(sessionId, {
      type: "turn_ended",
      turn_id: turnId,
      stop_reason: "cancelled",
    });
    if (current !== "closed" && current !== "idle") {
      this.appendEvent(sessionId, {
        type: "status_changed",
        from: current,
        to: "idle",
      });
    }
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

  async resumeSession(sessionId: SessionId): Promise<SessionSummary> {
    const found = this.store.sessions.find((s) => s.id === sessionId);
    if (!found) {
      throw new Error(`session '${sessionId}' not found`);
    }
    this.appendEvent(sessionId, {
      type: "status_changed",
      from: found.status,
      to: "idle",
    });
    return this.store.sessions.find((s) => s.id === sessionId) ?? found;
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
    this.appendEvent(gate.sessionId, {
      type: "status_changed",
      from: "waiting_approval",
      to: "running",
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

  private mockSchedules: Schedule[] = [];
  private mockScheduleRuns = new Map<string, ScheduleRun[]>();
  private mockGoals = new Map<string, Goal[]>();
  private mockDispatches: Dispatch[] = [];
  private mockMcpServers: McpServerRecord[] = [
    {
      id: "mcp_demo_github",
      space_id: "space_default",
      scope: "space",
      name: "github",
      transport: "streamable_http",
      args: [],
      url: "https://mcp.example.com/github",
      env_keys: ["GITHUB_TOKEN"],
      description: "Space 默认 GitHub MCP 演示项",
      enabled: true,
      created_by: "member_dev",
      created_at: nowMs() - 3600_000,
      updated_at: nowMs() - 3600_000,
    },
  ];
  private mockSkills: SkillRecord[] = [
    {
      id: "skl_demo_review",
      space_id: "space_default",
      scope: "space",
      name: "pr-reviewer",
      version: "1.0.0",
      description: "团队代码审查规范 Skill",
      tags: ["review", "rust"],
      instructions: "# PR Reviewer\n检查权限边界、错误路径与测试覆盖。",
      lifecycle: "active",
      enabled: true,
      created_by: "member_dev",
      created_at: nowMs() - 3600_000,
      updated_at: nowMs() - 3600_000,
    },
  ];

  async listSchedules(): Promise<Schedule[]> {
    return [...this.mockSchedules];
  }

  async createSchedule(req: NewSchedule): Promise<Schedule> {
    const now = nowMs();
    const s: Schedule = {
      id: makeId("sched"),
      space_id: "space_default",
      name: req.name,
      spec: req.spec,
      target: req.target,
      created_by: "member_dev",
      enabled: req.enabled ?? true,
      missed: req.missed ?? "run_once",
      overlap: req.overlap ?? "allow",
      next_run_at: now + 3600_000,
      created_at: now,
      updated_at: now,
    };
    this.mockSchedules.unshift(s);
    return s;
  }

  async setScheduleEnabled(
    scheduleId: ScheduleId,
    enabled: boolean
  ): Promise<Schedule> {
    const found = this.mockSchedules.find((s) => s.id === scheduleId);
    if (!found) throw new Error("schedule not found");
    found.enabled = enabled;
    found.updated_at = nowMs();
    return { ...found };
  }

  async triggerSchedule(scheduleId: ScheduleId): Promise<ScheduleRun> {
    const now = nowMs();
    const run: ScheduleRun = {
      id: makeId("srun"),
      schedule_id: scheduleId,
      scheduled_for: now,
      fired_at: now,
      outcome: {
        outcome: "delivered",
        session_id: this.store.sessions[0]?.id ?? "sess_mock",
        submit: { outcome: "started", turn_id: makeId("turn") },
      },
      manual: true,
    };
    const arr = this.mockScheduleRuns.get(scheduleId) ?? [];
    arr.unshift(run);
    this.mockScheduleRuns.set(scheduleId, arr);
    return run;
  }

  async listScheduleRuns(scheduleId: ScheduleId): Promise<ScheduleRun[]> {
    return [...(this.mockScheduleRuns.get(scheduleId) ?? [])];
  }

  async deleteSchedule(scheduleId: ScheduleId): Promise<void> {
    this.mockSchedules = this.mockSchedules.filter((s) => s.id !== scheduleId);
  }

  async listGoals(sessionId: SessionId): Promise<Goal[]> {
    return [...(this.mockGoals.get(sessionId) ?? [])];
  }

  async setGoal(sessionId: SessionId, req: NewGoal): Promise<Goal> {
    const now = nowMs();
    const g: Goal = {
      id: makeId("goal"),
      space_id: "space_default",
      session_id: sessionId,
      objective: req.objective,
      done_marker: "[[goal:done]]",
      max_turns: req.max_turns ?? 10,
      turns_used: 0,
      deadline_at: req.deadline_at,
      created_by: "member_dev",
      status: "active",
      created_at: now,
      updated_at: now,
    };
    const arr = this.mockGoals.get(sessionId) ?? [];
    arr.unshift(g);
    this.mockGoals.set(sessionId, arr);
    this.appendEvent(sessionId, { type: "goal_updated", goal: g });
    return g;
  }

  async stopGoal(goalId: GoalId): Promise<Goal> {
    for (const [sid, list] of this.mockGoals.entries()) {
      const found = list.find((g) => g.id === goalId);
      if (found) {
        found.status = "stopped";
        found.ended_reason = "stopped_by_member";
        found.updated_at = nowMs();
        this.appendEvent(sid, { type: "goal_updated", goal: { ...found } });
        return { ...found };
      }
    }
    throw new Error("goal not found");
  }

  async listDispatches(sessionId: SessionId): Promise<Dispatch[]> {
    return this.mockDispatches.filter(
      (d) => d.from_session === sessionId || d.to_session === sessionId
    );
  }

  async createDispatch(req: NewDispatch): Promise<Dispatch> {
    const toSession =
      req.target.kind === "session" ? req.target.session_id : makeId("sess");
    const d: Dispatch = {
      id: makeId("disp"),
      space_id: "space_default",
      from_session: req.from_session,
      to_session: toSession,
      member_id: "member_dev",
      depth: 1,
      created_session: req.target.kind === "new_session",
      submit: { outcome: "started", turn_id: makeId("turn") },
      created_at: nowMs(),
    };
    this.mockDispatches.unshift(d);
    this.appendEvent(req.from_session, { type: "dispatch_sent", dispatch: d });
    return d;
  }

  async listMcpServers(
    scope?: HubScopeLevel,
    workspaceId?: WorkspaceId
  ): Promise<McpServerRecord[]> {
    return this.mockMcpServers.filter(
      (s) =>
        (!scope || s.scope === scope) &&
        (!workspaceId || s.workspace_id === workspaceId)
    );
  }

  async createMcpServer(req: CreateMcpServerRequest): Promise<McpServerRecord> {
    const now = nowMs();
    const rec: McpServerRecord = {
      id: makeId("mcp"),
      space_id: "space_default",
      scope: req.scope,
      workspace_id: req.workspace_id,
      name: req.name,
      transport: req.transport,
      command: req.command,
      args: req.args ?? [],
      url: req.url,
      env_keys: req.env_keys ?? [],
      description: req.description ?? "",
      enabled: req.enabled ?? true,
      created_by: "member_dev",
      created_at: now,
      updated_at: now,
    };
    this.mockMcpServers.unshift(rec);
    return rec;
  }

  async updateMcpServer(
    id: string,
    req: UpdateMcpServerRequest
  ): Promise<McpServerRecord> {
    const rec = this.mockMcpServers.find((s) => s.id === id);
    if (!rec) throw new Error("mcp server not found");
    if (req.enabled !== undefined) rec.enabled = req.enabled;
    if (req.description !== undefined) rec.description = req.description;
    rec.updated_at = nowMs();
    return { ...rec };
  }

  async deleteMcpServer(id: string): Promise<void> {
    this.mockMcpServers = this.mockMcpServers.filter((s) => s.id !== id);
  }

  async probeMcpServer(id: string): Promise<McpProbeResult> {
    const rec = this.mockMcpServers.find((s) => s.id === id);
    if (!rec) throw new Error("mcp server not found");
    return {
      id: rec.id,
      name: rec.name,
      transport: rec.transport,
      reachable: true,
      detail: "mock probe ok",
    };
  }

  async listSkills(
    scope?: HubScopeLevel,
    workspaceId?: WorkspaceId
  ): Promise<SkillRecord[]> {
    return this.mockSkills.filter(
      (s) =>
        (!scope || s.scope === scope) &&
        (!workspaceId || s.workspace_id === workspaceId)
    );
  }

  async createSkill(req: CreateSkillRequest): Promise<SkillRecord> {
    const now = nowMs();
    const rec: SkillRecord = {
      id: makeId("skl"),
      space_id: "space_default",
      scope: req.scope,
      workspace_id: req.workspace_id,
      name: req.name,
      version: req.version ?? "1.0.0",
      description: req.description ?? "",
      tags: req.tags ?? [],
      instructions: req.instructions,
      lifecycle: req.lifecycle ?? "active",
      enabled: req.enabled ?? true,
      created_by: "member_dev",
      created_at: now,
      updated_at: now,
    };
    this.mockSkills.unshift(rec);
    return rec;
  }

  async updateSkill(id: string, req: UpdateSkillRequest): Promise<SkillRecord> {
    const rec = this.mockSkills.find((s) => s.id === id);
    if (!rec) throw new Error("skill not found");
    if (req.lifecycle !== undefined) rec.lifecycle = req.lifecycle;
    if (req.enabled !== undefined) rec.enabled = req.enabled;
    if (req.version !== undefined) rec.version = req.version;
    rec.updated_at = nowMs();
    return { ...rec };
  }

  async deleteSkill(id: string): Promise<void> {
    this.mockSkills = this.mockSkills.filter((s) => s.id !== id);
  }

  async getEffectiveHub(workspaceId?: WorkspaceId): Promise<EffectiveHubResponse> {
    const mcpMap = new Map<string, EffectiveHubResponse["mcp_servers"][number]>();
    for (const s of this.mockMcpServers) {
      if (s.scope === "space" && s.enabled) {
        mcpMap.set(s.name.toLowerCase(), { ...s, overridden_space_default: false });
      }
    }
    if (workspaceId) {
      for (const s of this.mockMcpServers) {
        if (s.scope === "workspace" && s.workspace_id === workspaceId && s.enabled) {
          mcpMap.set(s.name.toLowerCase(), {
            ...s,
            overridden_space_default: mcpMap.has(s.name.toLowerCase()),
          });
        }
      }
    }
    const mcp_servers = [...mcpMap.values()];
    const skills = this.mockSkills
      .filter((sk) => sk.enabled && sk.lifecycle !== "archived")
      .map((sk) => ({ ...sk, overridden_space_default: false }));
    return {
      space_id: "space_default",
      workspace_id: workspaceId,
      mcp_servers,
      skills,
      mcp_client_config: {
        mcpServers: Object.fromEntries(
          mcp_servers.map((m) => [m.name, { transport: m.transport, url: m.url, command: m.command }])
        ),
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
