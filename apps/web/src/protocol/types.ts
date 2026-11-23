/**
 * Re-exports of `crates/om-core` types generated via `ts-rs` into `../generated/`.
 * Never hand-duplicate wire DTOs here; run `cargo test -p om-core` to regenerate.
 */

export type { Action } from "../generated/Action";
export type { ApiError } from "../generated/ApiError";
export type { ApiErrorCode } from "../generated/ApiErrorCode";
export type { ApprovalDecision } from "../generated/ApprovalDecision";
export type { ApprovalId } from "../generated/ApprovalId";
export type { ApprovalOption } from "../generated/ApprovalOption";
export type { ApprovalOptionKind } from "../generated/ApprovalOptionKind";
export type { ApprovalRecord } from "../generated/ApprovalRecord";
export type { ApprovalStatus } from "../generated/ApprovalStatus";
export type { ClientMessage } from "../generated/ClientMessage";
export type { ContentBlock } from "../generated/ContentBlock";
export type { CreateSessionRequest } from "../generated/CreateSessionRequest";
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
export type { ListApprovalsResponse } from "../generated/ListApprovalsResponse";
export type { ListEventsQuery } from "../generated/ListEventsQuery";
export type { ListEventsResponse } from "../generated/ListEventsResponse";
export type { ListSessionsResponse } from "../generated/ListSessionsResponse";
export type { MemberId } from "../generated/MemberId";
export type { MissedRunPolicy } from "../generated/MissedRunPolicy";
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
export type { QueueRemoval } from "../generated/QueueRemoval";
export type { QueuedPrompt } from "../generated/QueuedPrompt";
export type { QueuedPromptId } from "../generated/QueuedPromptId";
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
export type { SkipReason } from "../generated/SkipReason";
export type { SpaceId } from "../generated/SpaceId";
export type { StopReason } from "../generated/StopReason";
export type { SubmitMode } from "../generated/SubmitMode";
export type { SubmitOutcome } from "../generated/SubmitOutcome";
export type { SubmitPromptRequest } from "../generated/SubmitPromptRequest";
export type { SubmitPromptResponse } from "../generated/SubmitPromptResponse";
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

import type { ContentBlock } from "../generated/ContentBlock";
import type { HarnessCapabilities } from "../generated/HarnessCapabilities";

export const WIRE_PROTOCOL_VERSION = 1;

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
    protocol: meta?.protocol ?? "acp",
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
