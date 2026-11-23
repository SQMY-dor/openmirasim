/**
 * Re-exports of `crates/om-core` types generated via `ts-rs` into `../generated/`.
 * Never hand-duplicate wire DTOs here; run `cargo test -p om-core` to regenerate.
 */

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
export type { EventPayload } from "../generated/EventPayload";
export type { HarnessCapabilities } from "../generated/HarnessCapabilities";
export type { ListApprovalsResponse } from "../generated/ListApprovalsResponse";
export type { ListEventsQuery } from "../generated/ListEventsQuery";
export type { ListEventsResponse } from "../generated/ListEventsResponse";
export type { ListSessionsResponse } from "../generated/ListSessionsResponse";
export type { MemberId } from "../generated/MemberId";
export type { PlanEntry } from "../generated/PlanEntry";
export type { PlanEntryStatus } from "../generated/PlanEntryStatus";
export type { Prompt } from "../generated/Prompt";
export type { ResolveApprovalRequest } from "../generated/ResolveApprovalRequest";
export type { SandboxMode } from "../generated/SandboxMode";
export type { ServerMessage } from "../generated/ServerMessage";
export type { SessionConfig } from "../generated/SessionConfig";
export type { SessionEvent } from "../generated/SessionEvent";
export type { SessionId } from "../generated/SessionId";
export type { SessionStatus } from "../generated/SessionStatus";
export type { SessionSummary } from "../generated/SessionSummary";
export type { SpaceId } from "../generated/SpaceId";
export type { StopReason } from "../generated/StopReason";
export type { SubmitPromptRequest } from "../generated/SubmitPromptRequest";
export type { SubmitPromptResponse } from "../generated/SubmitPromptResponse";
export type { ToolCallInfo } from "../generated/ToolCallInfo";
export type { ToolCallStatus } from "../generated/ToolCallStatus";
export type { ToolCallUpdate } from "../generated/ToolCallUpdate";
export type { ToolKind } from "../generated/ToolKind";
export type { TurnId } from "../generated/TurnId";
export type { WorkspaceId } from "../generated/WorkspaceId";

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

export interface HarnessCatalogEntry {
  id: string;
  label: string;
  protocol: "acp" | "stream_json" | "pty" | "gui_bridge" | "mock";
  capabilities: HarnessCapabilities;
}

export const KNOWN_HARNESSES: readonly HarnessCatalogEntry[] = [
  {
    id: "opencode",
    label: "OpenCode (ACP)",
    protocol: "acp",
    capabilities: {
      native_approval: true,
      steer: true,
      resume: true,
      structured_tools: true,
      pty_attach: false,
      remote_screen: false,
    },
  },
  {
    id: "dsh",
    label: "DeepSeek Harness (ACP)",
    protocol: "acp",
    capabilities: {
      native_approval: true,
      steer: true,
      resume: true,
      structured_tools: true,
      pty_attach: false,
      remote_screen: false,
    },
  },
  {
    id: "cc",
    label: "Claude Code (ACP / stream-json)",
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
    id: "codexcli",
    label: "Codex CLI (ACP / JSON)",
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
    label: "Mock Harness (Phase 0 验证)",
    protocol: "mock",
    capabilities: {
      native_approval: true,
      steer: true,
      resume: true,
      structured_tools: true,
      pty_attach: false,
      remote_screen: false,
    },
  },
];

export function renderContentBlockText(block: ContentBlock): string {
  if (block.type === "text") {
    return block.text;
  }
  return `[${block.name}](${block.uri})`;
}

export function renderContentBlocks(blocks: readonly ContentBlock[]): string {
  return blocks.map(renderContentBlockText).join("");
}
