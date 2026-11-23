import type {
  ApprovalDecision,
  ApprovalId,
  ApprovalRuleView,
  ConnectionState,
  CreateSessionRequest,
  HarnessDescriptor,
  QueuedPromptId,
  ServerInfo,
  SessionEvent,
  SessionId,
  SessionSummary,
  SubmitMode,
  SubmitOutcome,
  SubmitPromptResponse,
  WorkspaceSummary,
  WorktreeDiff,
  WorktreeEntry,
  WorktreeFileContent,
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

  /** List registered Git workspaces (`GET /api/v1/workspaces`). */
  listWorkspaces(): Promise<WorkspaceSummary[]>;

  /** Register a local Git repository as a workspace (`POST /api/v1/workspaces`). */
  registerWorkspace(name: string, repoPath: string): Promise<WorkspaceSummary>;

  /** List all persisted sessions (`GET /api/v1/sessions`). */
  listSessions(): Promise<SessionSummary[]>;

  /** Create a new session (`POST /api/v1/sessions`). */
  createSession(req: CreateSessionRequest): Promise<SessionSummary>;

  /** Close a session permanently (`DELETE /api/v1/sessions/:id`). */
  closeSession(sessionId: SessionId): Promise<void>;

  /** Fetch all historical events strictly after `afterSeq` (`GET /api/v1/sessions/:id/events`). */
  fetchEvents(sessionId: SessionId, afterSeq: number): Promise<SessionEvent[]>;

  /** Submit a prompt (`POST /api/v1/sessions/:id/prompts`, supports optional W2-3 `mode`). */
  submitPrompt(
    sessionId: SessionId,
    text: string,
    mode?: SubmitMode
  ): Promise<SubmitPromptResponse | SubmitOutcome>;

  /** Cancel an in-flight turn (`POST /api/v1/sessions/:id/cancel`). */
  cancelSession(sessionId: SessionId): Promise<void>;

  /** Resume a failed session (`POST /api/v1/sessions/:id/resume`). */
  resumeSession(sessionId: SessionId): Promise<SessionSummary>;

  /** Resolve a pending approval (`POST /api/v1/approvals/:id/resolve`). */
  resolveApproval(
    approvalId: ApprovalId,
    decision: ApprovalDecision
  ): Promise<void>;

  /** Withdraw a queued prompt (`DELETE /api/v1/sessions/:id/queue/:qid`). */
  withdrawQueued(sessionId: SessionId, queuedId: QueuedPromptId): Promise<void>;

  /** Move a queued prompt to `position` (`POST /api/v1/sessions/:id/queue/:qid/move`). */
  moveQueued(
    sessionId: SessionId,
    queuedId: QueuedPromptId,
    position: number
  ): Promise<void>;

  /** Resume a paused queue (`POST /api/v1/sessions/:id/queue/resume`). */
  resumeQueue(sessionId: SessionId): Promise<void>;

  /** Fetch live Git diff for a session's worktree (`GET /api/v1/sessions/:id/diff`). */
  getWorktreeDiff(sessionId: SessionId): Promise<WorktreeDiff>;

  /** List files in a session's worktree (`GET /api/v1/sessions/:id/files`). */
  listWorktreeFiles(sessionId: SessionId): Promise<WorktreeEntry[]>;

  /** Read a relative file inside a session's worktree (`GET /api/v1/sessions/:id/file`). */
  readWorktreeFile(
    sessionId: SessionId,
    path: string
  ): Promise<WorktreeFileContent>;

  /** List remembered approval rules on a session (`GET /api/v1/sessions/:id/approval-rules`). */
  listApprovalRules(sessionId: SessionId): Promise<ApprovalRuleView[]>;

  /** Clear remembered approval rules on a session (`DELETE /api/v1/sessions/:id/approval-rules`). */
  clearApprovalRules(sessionId: SessionId): Promise<number>;

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
