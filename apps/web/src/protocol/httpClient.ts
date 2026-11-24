import {
  computeBackoffMs,
  type EventSubscription,
  type EventSubscriptionHandlers,
  type OpenMirasimClient,
} from "./client";
import {
  WIRE_PROTOCOL_VERSION,
  type ApiError,
  type ApprovalDecision,
  type ApprovalId,
  type ApprovalRuleView,
  type AuditEntry,
  type ClearApprovalRulesResponse,
  type ClientMessage,
  type CreateSessionRequest,
  type DeviceTokenRecord,
  type HarnessDescriptor,
  type IssuedToken,
  type ListApprovalRulesResponse,
  type ListAuditResponse,
  type ListEventsResponse,
  type ListMembersResponse,
  type ListProvidersResponse,
  type ListSessionsResponse,
  type ListTokensResponse,
  type ListWorkspacesResponse,
  type ListWorktreeFilesResponse,
  type MemberId,
  type MemberRecord,
  type Principal,
  type ProviderInfo,
  type ProxyStats,
  type PushConfigResponse,
  type PushSubscriptionRecord,
  type PushTestResponse,
  type QueuedPromptId,
  type RegisterPushSubscriptionRequest,
  type RemoteScreenResponse,
  type ResolveApprovalRequest,
  type Role,
  type ServerInfo,
  type ServerMessage,
  type SessionEvent,
  type SessionId,
  type SessionSummary,
  type SubmitMode,
  type SubmitOutcome,
  type SubmitPromptResponse,
  type TokenId,
  type WorkspaceSummary,
  type WorktreeDiff,
  type WorktreeEntry,
  type WorktreeFileContent,
} from "./types";

const WS_PING_INTERVAL_MS = 25_000;

interface HealthResponse {
  status: string;
  version: string;
}

interface ListHarnessesResponse {
  harnesses: HarnessDescriptor[];
}

interface ListPushSubscriptionsResponse {
  subscriptions: PushSubscriptionRecord[];
}

export class HttpOpenMirasimClient implements OpenMirasimClient {
  readonly mode = "http" as const;
  readonly serverUrl: string;
  private readonly token: string;

  constructor(serverUrl: string, token: string) {
    this.serverUrl = serverUrl.replace(/\/+$/, "");
    this.token = token.trim();
  }

  private async request<T>(path: string, init?: RequestInit): Promise<T> {
    const res = await fetch(`${this.serverUrl}${path}`, {
      ...init,
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${this.token}`,
        ...(init?.headers ?? {}),
      },
    });

    if (!res.ok) {
      let detail = `HTTP ${res.status} ${res.statusText}`;
      try {
        const body = (await res.json()) as Partial<ApiError>;
        if (body.message) {
          detail = body.code ? `[${body.code}] ${body.message}` : body.message;
        }
      } catch {
        // ignore non-JSON error body
      }
      throw new Error(detail);
    }

    if (res.status === 204) {
      return undefined as T;
    }

    return (await res.json()) as T;
  }

  async connect(): Promise<ServerInfo> {
    await this.getMe();
    let serverVersion = "0.1.0";
    try {
      const res = await fetch(`${this.serverUrl}/healthz`);
      if (res.ok) {
        const health = (await res.json()) as Partial<HealthResponse>;
        if (health.version) {
          serverVersion = health.version;
        }
      }
    } catch {
      // ignore optional healthz metadata fetch failure
    }
    return {
      serverVersion,
      spaceLabel: "OpenMirasim Space",
    };
  }

  async getMe(): Promise<Principal> {
    return this.request<Principal>("/api/v1/me");
  }

  async listMembers(): Promise<MemberRecord[]> {
    const res = await this.request<ListMembersResponse>("/api/v1/members");
    return res.members;
  }

  async addMember(displayName: string, role: Role): Promise<MemberRecord> {
    return this.request<MemberRecord>("/api/v1/members", {
      method: "POST",
      body: JSON.stringify({ display_name: displayName, role }),
    });
  }

  async changeMemberRole(
    memberId: MemberId,
    role: Role
  ): Promise<MemberRecord> {
    return this.request<MemberRecord>(
      `/api/v1/members/${encodeURIComponent(memberId)}`,
      {
        method: "PATCH",
        body: JSON.stringify({ role }),
      }
    );
  }

  async disableMember(memberId: MemberId): Promise<MemberRecord> {
    return this.request<MemberRecord>(
      `/api/v1/members/${encodeURIComponent(memberId)}`,
      { method: "DELETE" }
    );
  }

  async listMemberTokens(memberId: MemberId): Promise<DeviceTokenRecord[]> {
    const res = await this.request<ListTokensResponse>(
      `/api/v1/members/${encodeURIComponent(memberId)}/tokens`
    );
    return res.tokens;
  }

  async issueMemberToken(
    memberId: MemberId,
    label: string,
    ttlSecs?: number
  ): Promise<IssuedToken> {
    return this.request<IssuedToken>(
      `/api/v1/members/${encodeURIComponent(memberId)}/tokens`,
      {
        method: "POST",
        body: JSON.stringify({
          label,
          ...(ttlSecs !== undefined ? { ttl_secs: ttlSecs } : {}),
        }),
      }
    );
  }

  async revokeToken(tokenId: TokenId): Promise<DeviceTokenRecord> {
    return this.request<DeviceTokenRecord>(
      `/api/v1/tokens/${encodeURIComponent(tokenId)}`,
      { method: "DELETE" }
    );
  }

  async listAudit(target?: string, limit = 100): Promise<AuditEntry[]> {
    const params = new URLSearchParams({ limit: String(limit) });
    if (target?.trim()) {
      params.set("target", target.trim());
    }
    const res = await this.request<ListAuditResponse>(
      `/api/v1/audit?${params.toString()}`
    );
    return res.entries;
  }

  async listProviders(): Promise<ProviderInfo[]> {
    const res = await this.request<ListProvidersResponse>("/api/v1/providers");
    return res.providers;
  }

  async getSessionProxyStats(sessionId: SessionId): Promise<ProxyStats> {
    return this.request<ProxyStats>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/proxy/stats`
    );
  }

  async getRemoteScreen(sessionId: SessionId): Promise<RemoteScreenResponse> {
    return this.request<RemoteScreenResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/remote-screen`
    );
  }

  async getPushConfig(): Promise<PushConfigResponse> {
    return this.request<PushConfigResponse>("/api/v1/push/config");
  }

  async listPushSubscriptions(): Promise<PushSubscriptionRecord[]> {
    const res = await this.request<ListPushSubscriptionsResponse>(
      "/api/v1/push/subscriptions"
    );
    return res.subscriptions;
  }

  async registerPushSubscription(
    req: RegisterPushSubscriptionRequest
  ): Promise<PushSubscriptionRecord> {
    return this.request<PushSubscriptionRecord>("/api/v1/push/subscriptions", {
      method: "POST",
      body: JSON.stringify(req),
    });
  }

  async deletePushSubscription(subId: string): Promise<void> {
    await this.request<void>(
      `/api/v1/push/subscriptions/${encodeURIComponent(subId)}`,
      { method: "DELETE" }
    );
  }

  async testPush(): Promise<PushTestResponse> {
    return this.request<PushTestResponse>("/api/v1/push/test", {
      method: "POST",
    });
  }

  async listHarnesses(): Promise<HarnessDescriptor[]> {
    const res = await this.request<ListHarnessesResponse>("/api/v1/harnesses");
    return res.harnesses;
  }

  async listWorkspaces(): Promise<WorkspaceSummary[]> {
    const res = await this.request<ListWorkspacesResponse>("/api/v1/workspaces");
    return res.workspaces;
  }

  async registerWorkspace(
    name: string,
    repoPath: string
  ): Promise<WorkspaceSummary> {
    return this.request<WorkspaceSummary>("/api/v1/workspaces", {
      method: "POST",
      body: JSON.stringify({ name, repo_path: repoPath }),
    });
  }

  async listSessions(): Promise<SessionSummary[]> {
    const res = await this.request<ListSessionsResponse>("/api/v1/sessions");
    return res.sessions;
  }

  async createSession(req: CreateSessionRequest): Promise<SessionSummary> {
    return this.request<SessionSummary>("/api/v1/sessions", {
      method: "POST",
      body: JSON.stringify(req),
    });
  }

  async closeSession(sessionId: SessionId): Promise<void> {
    await this.request<void>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}`,
      { method: "DELETE" }
    );
  }

  async fetchEvents(
    sessionId: SessionId,
    afterSeq: number
  ): Promise<SessionEvent[]> {
    const collected: SessionEvent[] = [];
    let cursor = afterSeq;
    let hasMore = true;

    while (hasMore) {
      const params = new URLSearchParams({ after_seq: String(cursor) });
      const page = await this.request<ListEventsResponse>(
        `/api/v1/sessions/${encodeURIComponent(sessionId)}/events?${params.toString()}`
      );
      collected.push(...page.events);
      if (page.events.length === 0) {
        break;
      }
      cursor = page.events[page.events.length - 1].seq;
      hasMore = page.has_more;
    }

    return collected;
  }

  async submitPrompt(
    sessionId: SessionId,
    text: string,
    mode?: SubmitMode
  ): Promise<SubmitPromptResponse | SubmitOutcome> {
    const body = {
      prompt: { content: [{ type: "text", text }] },
      ...(mode ? { mode } : {}),
    };
    return this.request<SubmitPromptResponse | SubmitOutcome>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/prompts`,
      {
        method: "POST",
        body: JSON.stringify(body),
      }
    );
  }

  async cancelSession(sessionId: SessionId): Promise<void> {
    await this.request<void>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/cancel`,
      { method: "POST" }
    );
  }

  async resumeSession(sessionId: SessionId): Promise<SessionSummary> {
    return this.request<SessionSummary>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/resume`,
      { method: "POST" }
    );
  }

  async resolveApproval(
    approvalId: ApprovalId,
    decision: ApprovalDecision
  ): Promise<void> {
    const body: ResolveApprovalRequest = { decision };
    await this.request<void>(
      `/api/v1/approvals/${encodeURIComponent(approvalId)}/resolve`,
      {
        method: "POST",
        body: JSON.stringify(body),
      }
    );
  }

  async withdrawQueued(
    sessionId: SessionId,
    queuedId: QueuedPromptId
  ): Promise<void> {
    await this.request<void>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/queue/${encodeURIComponent(queuedId)}`,
      { method: "DELETE" }
    );
  }

  async moveQueued(
    sessionId: SessionId,
    queuedId: QueuedPromptId,
    position: number
  ): Promise<void> {
    await this.request<void>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/queue/${encodeURIComponent(queuedId)}/move`,
      {
        method: "POST",
        body: JSON.stringify({ position }),
      }
    );
  }

  async resumeQueue(sessionId: SessionId): Promise<void> {
    await this.request<void>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/queue/resume`,
      { method: "POST" }
    );
  }

  async getWorktreeDiff(sessionId: SessionId): Promise<WorktreeDiff> {
    return this.request<WorktreeDiff>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/diff`
    );
  }

  async listWorktreeFiles(sessionId: SessionId): Promise<WorktreeEntry[]> {
    const res = await this.request<ListWorktreeFilesResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/files`
    );
    return res.entries;
  }

  async readWorktreeFile(
    sessionId: SessionId,
    path: string
  ): Promise<WorktreeFileContent> {
    const params = new URLSearchParams({ path });
    return this.request<WorktreeFileContent>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/file?${params.toString()}`
    );
  }

  async listApprovalRules(sessionId: SessionId): Promise<ApprovalRuleView[]> {
    const res = await this.request<ListApprovalRulesResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/approval-rules`
    );
    return res.rules;
  }

  async clearApprovalRules(sessionId: SessionId): Promise<number> {
    const res = await this.request<ClearApprovalRulesResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/approval-rules`,
      { method: "DELETE" }
    );
    return res.cleared;
  }

  async getPtyStatus(
    sessionId: SessionId
  ): Promise<import("./types").PtyStatus> {
    return this.request<import("./types").PtyStatus>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/pty`
    );
  }

  async killPty(sessionId: SessionId): Promise<void> {
    await this.request<void>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/pty`,
      { method: "DELETE" }
    );
  }

  async fetchPtyRecording(sessionId: SessionId): Promise<string> {
    const res = await fetch(
      `${this.serverUrl}/api/v1/sessions/${encodeURIComponent(sessionId)}/pty/recording`,
      {
        headers: {
          Authorization: `Bearer ${this.token}`,
        },
      }
    );
    if (!res.ok) {
      throw new Error(`获取终端录制失败 (HTTP ${res.status})`);
    }
    return await res.text();
  }

  buildPtyWsUrl(sessionId: SessionId, cols: number, rows: number): string {
    const wsBase = this.serverUrl.replace(/^http(s?):\/\//i, "ws$1://");
    const params = new URLSearchParams({
      token: this.token,
      cols: String(cols),
      rows: String(rows),
    });
    return `${wsBase}/ws/pty/${encodeURIComponent(sessionId)}?${params.toString()}`;
  }

  subscribeSession(
    sessionId: SessionId,
    getLastSeq: () => number,
    handlers: EventSubscriptionHandlers
  ): EventSubscription {
    let closed = false;
    let fatalError = false;
    let ws: WebSocket | null = null;
    let retryTimer: number | null = null;
    let pingTimer: number | null = null;
    let pingNonce = 1;
    let attempt = 0;

    const clearPing = () => {
      if (pingTimer !== null) {
        window.clearInterval(pingTimer);
        pingTimer = null;
      }
    };

    const sendFrame = (socket: WebSocket, msg: ClientMessage) => {
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify(msg));
      }
    };

    const buildWsUrl = (): string => {
      const wsBase = this.serverUrl.replace(/^http(s?):\/\//i, "ws$1://");
      const params = new URLSearchParams({ token: this.token });
      return `${wsBase}/ws/events?${params.toString()}`;
    };

    const scheduleReconnect = (reason: string) => {
      if (closed || fatalError) return;
      attempt += 1;
      handlers.onReconnectAttempt?.(attempt);
      handlers.onStateChange("reconnecting", reason);
      const delay = computeBackoffMs(attempt);
      retryTimer = window.setTimeout(() => {
        openSocket();
      }, delay);
    };

    const openSocket = () => {
      if (closed || fatalError) return;
      handlers.onStateChange(
        attempt === 0 ? "connecting" : "reconnecting",
        null
      );

      const socket = new WebSocket(buildWsUrl());
      ws = socket;

      socket.onopen = () => {
        if (closed) {
          socket.close();
          return;
        }
        handlers.onStateChange("catching_up", null);
        sendFrame(socket, {
          type: "hello",
          protocol_version: WIRE_PROTOCOL_VERSION,
        });
        clearPing();
        pingTimer = window.setInterval(() => {
          sendFrame(socket, { type: "ping", nonce: pingNonce++ });
        }, WS_PING_INTERVAL_MS);
      };

      socket.onmessage = (raw) => {
        if (closed || typeof raw.data !== "string") return;
        let msg: ServerMessage;
        try {
          msg = JSON.parse(raw.data) as ServerMessage;
        } catch {
          return;
        }

        switch (msg.type) {
          case "welcome": {
            sendFrame(socket, {
              type: "subscribe",
              session_id: sessionId,
              last_seq: getLastSeq(),
            });
            break;
          }
          case "subscribed": {
            if (msg.session_id !== sessionId) return;
            attempt = 0;
            handlers.onHeadSeq?.(msg.head_seq);
            handlers.onStateChange("connected", null);
            break;
          }
          case "event": {
            handlers.onEvent(msg.event);
            break;
          }
          case "resync": {
            if (msg.session_id !== sessionId) return;
            handlers.onResync?.();
            handlers.onStateChange("catching_up", null);
            sendFrame(socket, {
              type: "subscribe",
              session_id: sessionId,
              last_seq: getLastSeq(),
            });
            break;
          }
          case "error": {
            const isFatal =
              msg.error.code === "not_found" ||
              msg.error.code === "unauthorized" ||
              msg.error.code === "forbidden" ||
              msg.error.code === "unsupported";
            if (isFatal) {
              fatalError = true;
              clearPing();
            }
            handlers.onStateChange(
              "error",
              `[${msg.error.code}] ${msg.error.message}`
            );
            break;
          }
          case "pong":
            break;
        }
      };

      socket.onerror = () => {
        if (closed || fatalError) return;
        handlers.onStateChange("reconnecting", "WebSocket 连接异常，正在重连");
      };

      socket.onclose = () => {
        clearPing();
        if (closed || fatalError) return;
        scheduleReconnect("连接已断开，等待自动重连");
      };
    };

    openSocket();

    return {
      close: () => {
        closed = true;
        clearPing();
        if (retryTimer !== null) {
          window.clearTimeout(retryTimer);
        }
        if (ws) {
          sendFrame(ws, { type: "unsubscribe", session_id: sessionId });
          ws.onopen = null;
          ws.onmessage = null;
          ws.onerror = null;
          ws.onclose = null;
          ws.close();
        }
        handlers.onStateChange("disconnected", null);
      },
    };
  }
}
