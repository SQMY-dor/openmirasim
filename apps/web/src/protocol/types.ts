/**
 * Re-exports of `crates/om-core` types generated via `ts-rs` into `../generated/`.
 * Never hand-duplicate wire DTOs here; run `cargo test -p om-core` to regenerate.
 */

export type { Action } from "../generated/Action";
export type { AddMemberRequest } from "../generated/AddMemberRequest";
export type { ApiError } from "../generated/ApiError";
export type { ApiErrorCode } from "../generated/ApiErrorCode";
export type { ApprovalDecision } from "../generated/ApprovalDecision";
export type { ApprovalId } from "../generated/ApprovalId";
export type { ApprovalOption } from "../generated/ApprovalOption";
export type { ApprovalOptionKind } from "../generated/ApprovalOptionKind";
export type { ApprovalRecord } from "../generated/ApprovalRecord";
export type { ApprovalRule } from "../generated/ApprovalRule";
export type { ApprovalStatus } from "../generated/ApprovalStatus";
export type { AuditEntry } from "../generated/AuditEntry";
export type { ChangeRoleRequest } from "../generated/ChangeRoleRequest";
export type { ClearApprovalRulesResponse } from "../generated/ClearApprovalRulesResponse";
export type { ClientMessage } from "../generated/ClientMessage";
export type { ContentBlock } from "../generated/ContentBlock";
export type { CreateSessionRequest } from "../generated/CreateSessionRequest";
export type { DeviceTokenRecord } from "../generated/DeviceTokenRecord";
export type { Dispatch } from "../generated/Dispatch";
export type { DispatchId } from "../generated/DispatchId";
export type { DispatchTarget } from "../generated/DispatchTarget";
export type { EventPayload } from "../generated/EventPayload";
export type { FileChange } from "../generated/FileChange";
export type { FileChangeKind } from "../generated/FileChangeKind";
export type { Goal } from "../generated/Goal";
export type { GoalId } from "../generated/GoalId";
export type { GoalStatus } from "../generated/GoalStatus";
export type { HarnessCapabilities } from "../generated/HarnessCapabilities";
export type { IssueTokenRequest } from "../generated/IssueTokenRequest";
export type { IssuedToken } from "../generated/IssuedToken";
export type { ListApprovalRulesResponse } from "../generated/ListApprovalRulesResponse";
export type { ListApprovalsResponse } from "../generated/ListApprovalsResponse";
export type { ListAuditResponse } from "../generated/ListAuditResponse";
export type { ListDispatchesResponse } from "../generated/ListDispatchesResponse";
export type { ListEventsQuery } from "../generated/ListEventsQuery";
export type { ListEventsResponse } from "../generated/ListEventsResponse";
export type { ListGoalsResponse } from "../generated/ListGoalsResponse";
export type { ListMembersResponse } from "../generated/ListMembersResponse";
export type { ListProvidersResponse } from "../generated/ListProvidersResponse";
export type { ListQueueResponse } from "../generated/ListQueueResponse";
export type { ListScheduleRunsResponse } from "../generated/ListScheduleRunsResponse";
export type { ListSchedulesResponse } from "../generated/ListSchedulesResponse";
export type { ListSessionsResponse } from "../generated/ListSessionsResponse";
export type { ListTokensResponse } from "../generated/ListTokensResponse";
export type { ListWorkspacesResponse } from "../generated/ListWorkspacesResponse";
export type { ListWorktreeFilesResponse } from "../generated/ListWorktreeFilesResponse";
export type { MemberId } from "../generated/MemberId";
export type { MemberRecord } from "../generated/MemberRecord";
export type { MissedRunPolicy } from "../generated/MissedRunPolicy";
export type { MoveQueuedRequest } from "../generated/MoveQueuedRequest";
export type { NewDispatch } from "../generated/NewDispatch";
export type { NewGoal } from "../generated/NewGoal";
export type { NewSchedule } from "../generated/NewSchedule";
export type { OverlapPolicy } from "../generated/OverlapPolicy";
export type { PlanEntry } from "../generated/PlanEntry";
export type { PlanEntryStatus } from "../generated/PlanEntryStatus";
export type { Principal } from "../generated/Principal";
export type { Prompt } from "../generated/Prompt";
export type { ProviderInfo } from "../generated/ProviderInfo";
export type { ProviderKind } from "../generated/ProviderKind";
export type { ProxyStats } from "../generated/ProxyStats";
export type { QueueRemoval } from "../generated/QueueRemoval";
export type { QueuedPrompt } from "../generated/QueuedPrompt";
export type { QueuedPromptId } from "../generated/QueuedPromptId";
export type { RegisterWorkspaceRequest } from "../generated/RegisterWorkspaceRequest";
export type { ResolveApprovalRequest } from "../generated/ResolveApprovalRequest";
export type { Role } from "../generated/Role";
export type { RunOutcome } from "../generated/RunOutcome";
export type { SandboxMode } from "../generated/SandboxMode";
export type { Schedule } from "../generated/Schedule";
export type { ScheduleId } from "../generated/ScheduleId";
export type { ScheduleRun } from "../generated/ScheduleRun";
export type { ScheduleRunId } from "../generated/ScheduleRunId";
export type { ScheduleSpec } from "../generated/ScheduleSpec";
export type { ScheduleTarget } from "../generated/ScheduleTarget";
export type { ServerMessage } from "../generated/ServerMessage";
export type { SessionConfig } from "../generated/SessionConfig";
export type { SessionEvent } from "../generated/SessionEvent";
export type { SessionId } from "../generated/SessionId";
export type { SessionStatus } from "../generated/SessionStatus";
export type { SessionSummary } from "../generated/SessionSummary";
export type { SetScheduleEnabledRequest } from "../generated/SetScheduleEnabledRequest";
export type { SkipReason } from "../generated/SkipReason";
export type { SpaceId } from "../generated/SpaceId";
export type { StopReason } from "../generated/StopReason";
export type { SubmitMode } from "../generated/SubmitMode";
export type { SubmitOutcome } from "../generated/SubmitOutcome";
export type { SubmitPromptRequest } from "../generated/SubmitPromptRequest";
export type { SubmitPromptResponse } from "../generated/SubmitPromptResponse";
export type { TokenId } from "../generated/TokenId";
export type { ToolCallInfo } from "../generated/ToolCallInfo";
export type { ToolCallStatus } from "../generated/ToolCallStatus";
export type { ToolCallUpdate } from "../generated/ToolCallUpdate";
export type { ToolKind } from "../generated/ToolKind";
export type { TurnId } from "../generated/TurnId";
export type { Weekday } from "../generated/Weekday";
export type { WorkspaceId } from "../generated/WorkspaceId";
export type { WorkspaceSummary } from "../generated/WorkspaceSummary";
export type { WorktreeDiff } from "../generated/WorktreeDiff";
export type { WorktreeEntry } from "../generated/WorktreeEntry";
export type { WorktreeInfo } from "../generated/WorktreeInfo";

import type { Action } from "../generated/Action";
import type { ApprovalRule } from "../generated/ApprovalRule";
import type { ContentBlock } from "../generated/ContentBlock";
import type { HarnessCapabilities } from "../generated/HarnessCapabilities";
import type { Role } from "../generated/Role";

export const WIRE_PROTOCOL_VERSION = 1;
export const MAX_MEMBERS_PER_SPACE = 30;

export type ConnectionState =
  | "disconnected"
  | "connecting"
  | "catching_up"
  | "connected"
  | "reconnecting"
  | "error";

export interface StreamDiagnostics {
  connectionState: ConnectionState;
  lastSeq: number;
  headSeq: number;
  reconnectCount: number;
  resyncCount: number;
  dedupedCount: number;
  gapDetectedCount: number;
  lastError: string | null;
}

export interface ServerInfo {
  serverVersion: string;
  spaceLabel: string;
}

/** Matches `GET /api/v1/harnesses` (`om_server::harness::HarnessDescriptor`). */
export interface HarnessDescriptor {
  id: string;
  capabilities: HarnessCapabilities;
}

/** Alias to generated `ApprovalRule` for backward compatibility. */
export type ApprovalRuleView = ApprovalRule;

/** Matches `GET /api/v1/sessions/:id/file?path=...`. */
export interface WorktreeFileContent {
  path: string;
  content: string;
  utf8: boolean;
  size: number;
}

/** Matches `GET /api/v1/sessions/:id/remote-screen`. */
export interface RemoteScreenInfo {
  available: boolean;
  bridge_session_id: string;
  takeover_url: string;
  display: string;
  driver: string;
  profile_dir: string;
  status: string;
}

export interface RemoteScreenResponse {
  available: boolean;
  screen?: RemoteScreenInfo;
}

/** Matches `/api/v1/push/*` (W2-4 PWA Web Push). */
export interface PushConfigResponse {
  enabled: boolean;
  vapid_public_key?: string;
  subscription_count: number;
}

export interface PushSubscriptionRecord {
  id: string;
  member_id: string;
  endpoint: string;
  keys: { p256dh: string; auth: string };
  user_agent?: string;
  created_at: number;
}

export interface RegisterPushSubscriptionRequest {
  endpoint: string;
  keys: { p256dh: string; auth: string };
  user_agent?: string;
}

export interface PushTestResponse {
  delivered: number;
  mode: string;
  message: string;
}

/** Matches `GET /api/v1/sessions/:id/pty` (Phase 3 native PTY runtime). */
export interface PtyStatus {
  active: boolean;
  cols: number;
  rows: number;
  has_recording: boolean;
  recording_bytes: number;
  clients: number;
}

/** Saved server connection profile (shared by Web and Tauri v2 desktop shell). */
export interface ServerProfile {
  id: string;
  name: string;
  serverUrl: string;
  lastUsedAt: number;
}

const ROLE_RANK: Record<Role, number> = {
  viewer: 0,
  member: 1,
  admin: 2,
  owner: 3,
};

export function roleCan(role: Role, action: Action): boolean {
  const rank = ROLE_RANK[role];
  switch (action) {
    case "view_sessions":
      return true;
    case "create_session":
    case "prompt_session":
    case "resolve_approval":
    case "manage_approval_rules":
      return rank >= ROLE_RANK.member;
    case "manage_workspaces":
    case "manage_members":
    case "view_audit":
      return rank >= ROLE_RANK.admin;
    case "manage_space":
      return role === "owner";
  }
}

export function roleCanManage(actor: Role, target: Role): boolean {
  return (
    roleCan(actor, "manage_members") &&
    (actor === "owner" || ROLE_RANK[target] < ROLE_RANK[actor])
  );
}

export interface HarnessCatalogEntry {
  id: string;
  label: string;
  protocol: "acp" | "stream_json" | "pty" | "gui_bridge" | "mock";
  capabilities: HarnessCapabilities;
}

const HARNESS_DISPLAY_META: Record<
  string,
  { label: string; protocol: HarnessCatalogEntry["protocol"] }
> = {
  opencode: { label: "OpenCode (ACP)", protocol: "acp" },
  dsh: { label: "DeepSeek Harness (ACP)", protocol: "acp" },
  cc: { label: "Claude Code (stream-json)", protocol: "stream_json" },
  codexcli: { label: "Codex CLI (JSON)", protocol: "stream_json" },
  "antigravity-gui": {
    label: "Antigravity GUI (Xvfb / noVNC)",
    protocol: "gui_bridge",
  },
  mock: { label: "Mock Harness (内置验证)", protocol: "mock" },
  "fake-acp": { label: "Fake ACP Agent (测试)", protocol: "acp" },
};

/** Default fallback catalog used only in local mock demo mode. */
export const KNOWN_HARNESSES: readonly HarnessCatalogEntry[] = [
  {
    id: "opencode",
    label: "OpenCode (ACP)",
    protocol: "acp",
    capabilities: {
      native_approval: true,
      steer: false,
      resume: true,
      structured_tools: true,
      pty_attach: false,
      remote_screen: false,
    },
  },
  {
    id: "mock",
    label: "Mock Harness (内置验证)",
    protocol: "mock",
    capabilities: {
      native_approval: true,
      steer: false,
      resume: true,
      structured_tools: true,
      pty_attach: false,
      remote_screen: false,
    },
  },
];

export function describeHarness(desc: HarnessDescriptor): HarnessCatalogEntry {
  const meta = HARNESS_DISPLAY_META[desc.id];
  return {
    id: desc.id,
    label: meta?.label ?? desc.id,
    protocol: meta?.protocol ?? (desc.capabilities.remote_screen ? "gui_bridge" : "acp"),
    capabilities: desc.capabilities,
  };
}

export function renderContentBlockText(block: ContentBlock): string {
  if (block.type === "text") {
    return block.text;
  }
  return `[${block.name}](${block.uri})`;
}

export function renderContentBlocks(blocks: readonly ContentBlock[]): string {
  return blocks.map(renderContentBlockText).join("");
}

export function isSafeHttpUrl(raw: string | undefined | null): boolean {
  if (!raw) return false;
  try {
    const parsed = new URL(raw);
    return parsed.protocol === "http:" || parsed.protocol === "https:";
  } catch {
    return false;
  }
}

/** Phase 4: Space / Workspace two-tier MCP Hub & Skills lifecycle types. */
export type HubScopeLevel = "space" | "workspace";
export type McpTransport = "stdio" | "sse" | "streamable_http";
export type SkillLifecycle = "draft" | "active" | "deprecated" | "archived";

export interface McpServerRecord {
  id: string;
  space_id: string;
  scope: HubScopeLevel;
  workspace_id?: string;
  owner_member_id?: string;
  global_available?: boolean;
  name: string;
  transport: McpTransport;
  command?: string;
  args: string[];
  url?: string;
  env_keys: string[];
  description: string;
  enabled: boolean;
  created_by: string;
  created_at: number;
  updated_at: number;
}

export interface CreateMcpServerRequest {
  scope: HubScopeLevel;
  workspace_id?: string;
  name: string;
  transport: McpTransport;
  command?: string;
  args?: string[];
  url?: string;
  env_keys?: string[];
  description?: string;
  enabled?: boolean;
  global_available?: boolean;
}

export interface UpdateMcpServerRequest {
  enabled?: boolean;
  global_available?: boolean;
  description?: string;
  command?: string;
  args?: string[];
  url?: string;
  env_keys?: string[];
}

export interface McpProbeResult {
  id: string;
  name: string;
  transport: McpTransport;
  reachable: boolean;
  detail: string;
}

export interface SkillRecord {
  id: string;
  space_id: string;
  scope: HubScopeLevel;
  workspace_id?: string;
  name: string;
  version: string;
  description: string;
  tags: string[];
  instructions: string;
  lifecycle: SkillLifecycle;
  enabled: boolean;
  created_by: string;
  created_at: number;
  updated_at: number;
}

export interface CreateSkillRequest {
  scope: HubScopeLevel;
  workspace_id?: string;
  name: string;
  version?: string;
  description?: string;
  tags?: string[];
  instructions: string;
  lifecycle?: SkillLifecycle;
  enabled?: boolean;
}

export interface UpdateSkillRequest {
  version?: string;
  description?: string;
  tags?: string[];
  instructions?: string;
  lifecycle?: SkillLifecycle;
  enabled?: boolean;
}

export interface EffectiveMcpServer extends McpServerRecord {
  overridden_space_default: boolean;
}

export interface EffectiveSkill extends SkillRecord {
  overridden_space_default: boolean;
}

export interface EffectiveHubResponse {
  space_id: string;
  workspace_id?: string;
  mcp_servers: EffectiveMcpServer[];
  skills: EffectiveSkill[];
  mcp_client_config: Record<string, unknown>;
}

