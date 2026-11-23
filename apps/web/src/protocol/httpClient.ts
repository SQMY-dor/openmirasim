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
  type ClientMessage,
  type CreateSessionRequest,
  type ListEventsResponse,
  type ListSessionsResponse,
  type ResolveApprovalRequest,
  type ServerInfo,
  type ServerMessage,
  type SessionEvent,
  type SessionId,
  type SessionSummary,
  type SubmitPromptRequest,
  type SubmitPromptResponse,
} from "./types";

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
    // Validate Bearer token via GET /api/v1/sessions (Phase 0 minimal REST surface).
    await this.listSessions();
    return {
      serverVersion: "0.1.0",
      spaceLabel: "OpenMirasim Space",
    };
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
    text: string
  ): Promise<SubmitPromptResponse> {
    const body: SubmitPromptRequest = {
      prompt: { content: [{ type: "text", text }] },
    };
    return this.request<SubmitPromptResponse>(
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

  subscribeSession(
    sessionId: SessionId,
    getLastSeq: () => number,
    handlers: EventSubscriptionHandlers
  ): EventSubscription {
    let closed = false;
    let ws: WebSocket | null = null;
    let retryTimer: number | null = null;
    let attempt = 0;

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
      if (closed) return;
      attempt += 1;
      handlers.onReconnectAttempt?.(attempt);
      handlers.onStateChange("reconnecting", reason);
      const delay = computeBackoffMs(attempt);
      retryTimer = window.setTimeout(() => {
        openSocket();
      }, delay);
    };

    const openSocket = () => {
      if (closed) return;
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
        if (closed) return;
        handlers.onStateChange("reconnecting", "WebSocket 连接异常，正在重连");
      };

      socket.onclose = () => {
        if (closed) return;
        scheduleReconnect("连接已断开，等待自动重连");
      };
    };

    openSocket();

    return {
      close: () => {
        closed = true;
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
