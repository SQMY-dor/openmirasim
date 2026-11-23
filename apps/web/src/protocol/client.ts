import type {
  ApprovalDecision,
  ApprovalId,
  ConnectionState,
  CreateSessionRequest,
  HarnessDescriptor,
  ServerInfo,
  SessionEvent,
  SessionId,
  SessionSummary,
  SubmitPromptResponse,
} from "./types";

export interface EventSubscriptionHandlers {
  onEvent: (record: SessionEvent) => void;
  onStateChange: (state: ConnectionState, error?: string | null) => void;
  onHeadSeq?: (headSeq: number) => void;
  onReconnectAttempt?: (attempt: number) => void;
  onResync?: () => void;
}

export interface EventSubscription {
  close: () => void;
}

export interface OpenMirasimClient {
  readonly mode: "http" | "mock";
  readonly serverUrl: string;

  /** Verify token against the server and return server metadata. */
  connect(): Promise<ServerInfo>;

  /** List registered harnesses and their capabilities (`GET /api/v1/harnesses`). */
  listHarnesses(): Promise<HarnessDescriptor[]>;

  /** List all persisted sessions (`GET /api/v1/sessions`). */
  listSessions(): Promise<SessionSummary[]>;

  /** Create a new session (`POST /api/v1/sessions`). */
  createSession(req: CreateSessionRequest): Promise<SessionSummary>;

  /** Close a session permanently (`DELETE /api/v1/sessions/:id`). */
  closeSession(sessionId: SessionId): Promise<void>;

  /** Fetch all historical events strictly after `afterSeq` (`GET /api/v1/sessions/:id/events`). */
  fetchEvents(sessionId: SessionId, afterSeq: number): Promise<SessionEvent[]>;

  /** Submit a prompt (`POST /api/v1/sessions/:id/prompts`). */
  submitPrompt(
    sessionId: SessionId,
    text: string
  ): Promise<SubmitPromptResponse>;

  /** Cancel an in-flight turn (`POST /api/v1/sessions/:id/cancel`). */
  cancelSession(sessionId: SessionId): Promise<void>;

  /** Resume a failed session (`POST /api/v1/sessions/:id/resume`). */
  resumeSession(sessionId: SessionId): Promise<SessionSummary>;

  /** Resolve a pending approval (`POST /api/v1/approvals/:id/resolve`). */
  resolveApproval(
    approvalId: ApprovalId,
    decision: ApprovalDecision
  ): Promise<void>;

  /**
   * Subscribe to `/ws/events` using the `Hello -> Welcome -> Subscribe { last_seq }` handshake.
   */
  subscribeSession(
    sessionId: SessionId,
    getLastSeq: () => number,
    handlers: EventSubscriptionHandlers
  ): EventSubscription;

  /** Optional local demo helper to simulate network disconnect + `last_seq` catchup. */
  simulateNetworkDrop?(sessionId: SessionId): Promise<void>;
}

export function computeBackoffMs(attempt: number): number {
  const base = 600;
  const max = 12_000;
  return Math.min(max, Math.round(base * Math.pow(1.6, Math.max(0, attempt - 1))));
}
