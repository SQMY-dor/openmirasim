import {
  AlertTriangle,
  ArrowDownLeft,
  ArrowUpRight,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Circle,
  Clock,
  Compass,
  ListChecks,
  ShieldAlert,
  Target,
  Wrench,
  XCircle,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { ProjectedToolCall, TimelineItem } from "../protocol/reducer";
import type {
  ApprovalDecision,
  ApprovalId,
  ApprovalRecord,
  PlanEntry,
} from "../protocol/types";
import { MarkdownContent } from "./MarkdownContent";

interface SessionStreamProps {
  harnessId: string;
  items: TimelineItem[];
  onResolveApproval: (
    approvalId: ApprovalId,
    decision: ApprovalDecision
  ) => Promise<void>;
}

function formatTime(tsMs: number): string {
  try {
    return new Date(tsMs).toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
  } catch {
    return "";
  }
}

function PlanStrip({ entries }: { entries: PlanEntry[] }) {
  if (entries.length === 0) return null;
  const doneCount = entries.filter((e) => e.status === "completed").length;

  return (
    <div className="plan-card" data-plan-strip="true">
      <div className="plan-head">
        <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
          <ListChecks size={13} />
          <span>执行计划</span>
        </span>
        <span className="chip">
          {doneCount}/{entries.length}
        </span>
      </div>
      <ul className="plan-list">
        {entries.map((entry, idx) => (
          <li key={`plan-${idx}`} className={`plan-item ${entry.status}`}>
            {entry.status === "completed" ? (
              <CheckCircle2 size={13} />
            ) : entry.status === "in_progress" ? (
              <Clock size={13} />
            ) : (
              <Circle size={12} />
            )}
            <span>{entry.content}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function ToolCallCard({ tool }: { tool: ProjectedToolCall }) {
  const [expanded, setExpanded] = useState(
    tool.status === "in_progress" || tool.status === "pending"
  );

  const chipClass =
    tool.status === "completed"
      ? "ok"
      : tool.status === "failed"
        ? "err"
        : "warn";

  return (
    <div className="tool-card" data-tool-call-id={tool.tool_call_id}>
      <button
        type="button"
        className="tool-card-header"
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={expanded}
      >
        <span className="tool-card-title">
          {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
          <Wrench size={13} />
          <span>{tool.title}</span>
          <span className="chip">{tool.kind}</span>
        </span>
        <span className={`chip ${chipClass}`}>
          {tool.status === "completed" && <CheckCircle2 size={11} />}
          {(tool.status === "in_progress" || tool.status === "pending") && (
            <Clock size={11} />
          )}
          {tool.status === "failed" && <XCircle size={11} />}
          <span>{tool.status}</span>
        </span>
      </button>

      {expanded && (
        <div className="tool-card-body">
          {tool.locations.length > 0 && (
            <div>
              <div className="tool-section-label">Locations</div>
              <pre className="tool-pre">{tool.locations.join("\n")}</pre>
            </div>
          )}
          {tool.rawInput && (
            <div>
              <div className="tool-section-label">Input</div>
              <pre className="tool-pre">{tool.rawInput}</pre>
            </div>
          )}
          {tool.outputText && (
            <div>
              <div className="tool-section-label">Output</div>
              <pre className="tool-pre">{tool.outputText}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function ApprovalCard({
  approval,
  onResolve,
}: {
  approval: ApprovalRecord;
  onResolve: (
    approvalId: ApprovalId,
    decision: ApprovalDecision
  ) => Promise<void>;
}) {
  const [submitting, setSubmitting] = useState(false);

  const handleSelectOption = async (optionId: string) => {
    setSubmitting(true);
    try {
      await onResolve(approval.id, {
        outcome: "selected",
        option_id: optionId,
      });
    } finally {
      setSubmitting(false);
    }
  };

  const resolved = approval.status !== "pending";
  const selectedOptionId =
    approval.decision?.outcome === "selected"
      ? approval.decision.option_id
      : undefined;
  const selectedOpt = selectedOptionId
    ? approval.options.find((o) => o.option_id === selectedOptionId)
    : undefined;
  const rejected =
    approval.status === "cancelled" ||
    (selectedOpt !== undefined &&
      (selectedOpt.kind === "reject_once" ||
        selectedOpt.kind === "reject_always"));

  return (
    <div
      className={`approval-card ${resolved ? (rejected ? "rejected" : "resolved") : ""}`}
      data-approval-id={approval.id}
    >
      <div className="approval-head">
        <span className="approval-title">
          <ShieldAlert size={15} />
          <span>工具执行审批</span>
          {approval.tool_kind && <span className="chip">{approval.tool_kind}</span>}
        </span>
        <span
          className={`chip ${
            resolved ? (rejected ? "err" : "ok") : "warn"
          }`}
        >
          {resolved ? (rejected ? "已拒绝" : "已放行") : "待审批"}
        </span>
      </div>

      <p className="approval-summary">{approval.title}</p>

      {approval.locations && approval.locations.length > 0 && (
        <pre className="tool-pre" style={{ marginBottom: 8 }}>
          {approval.locations.join("\n")}
        </pre>
      )}

      {resolved ? (
        <div className="approval-resolved-note">
          决策：{selectedOpt ? selectedOpt.name : approval.decision?.outcome ?? approval.status}
          {approval.auto_rule ? ` · 策略规则：${approval.auto_rule}` : ""}
          {approval.resolved_by ? ` · 处理人：${approval.resolved_by}` : ""}
          {approval.resolved_at ? ` · ${formatTime(approval.resolved_at)}` : ""}
        </div>
      ) : (
        <div className="approval-actions">
          {approval.options.map((opt) => {
            const isAllow =
              opt.kind === "allow_once" || opt.kind === "allow_always";
            return (
              <button
                key={opt.option_id}
                type="button"
                className={`btn btn-sm ${
                  opt.kind === "allow_once"
                    ? "btn-primary"
                    : isAllow
                      ? ""
                      : "btn-danger"
                }`}
                disabled={submitting}
                data-approval-kind={opt.kind}
                onClick={() => void handleSelectOption(opt.option_id)}
              >
                {opt.name}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

function ThinkingStrip({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="thinking-block">
      <button
        type="button"
        className="thinking-toggle"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
      >
        {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        <span>推理过程（{text.length} 字）</span>
      </button>
      {open && <p className="thinking-body">{text}</p>}
    </div>
  );
}

export function SessionStream({
  harnessId,
  items,
  onResolveApproval,
}: SessionStreamProps) {
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const pinnedToBottomRef = useRef(true);

  const handleScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    const distance = el.scrollHeight - el.scrollTop - el.clientHeight;
    pinnedToBottomRef.current = distance < 72;
  };

  useEffect(() => {
    const el = scrollRef.current;
    if (!el || !pinnedToBottomRef.current) return;
    el.scrollTop = el.scrollHeight;
  }, [items]);

  if (items.length === 0) {
    return (
      <div className="empty-stage">
        <h2>准备就绪 · {harnessId}</h2>
        <p>
          在下方输入第一条指令启动 Agent 回合。所有事件按单调递增 <code>seq</code>{" "}
          持久化在中心服务器，刷新页面或断网后自动续传。
        </p>
      </div>
    );
  }

  return (
    <div
      className="stream-scroll"
      ref={scrollRef}
      onScroll={handleScroll}
      id="session-stream"
    >
      <div className="stream-inner">
        {items.map((item) => {
          if (item.kind === "user") {
            return (
              <article key={item.id} className="turn-user" data-turn-kind="user">
                <div className="turn-user-meta">
                  <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                    {item.steered && <Compass size={12} />}
                    <span>{item.author}</span>
                    {item.steered && <span className="chip warn">Steer</span>}
                  </span>
                  <span>
                    seq #{item.seq} · {formatTime(item.ts)}
                  </span>
                </div>
                <p className="turn-user-text">{item.text}</p>
              </article>
            );
          }

          if (item.kind === "goal") {
            const g = item.goal;
            const statusClass =
              g.status === "achieved"
                ? "ok"
                : g.status === "active"
                  ? "warn"
                  : "err";
            return (
              <div key={item.id} className="plan-card" data-goal-id={g.id}>
                <div className="plan-head">
                  <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                    <Target size={13} />
                    <span>目标驱动循环</span>
                  </span>
                  <span className={`chip ${statusClass}`}>
                    {g.status} · {g.turns_used}/{g.max_turns} 轮
                  </span>
                </div>
                <p style={{ margin: "6px 0 0", fontSize: 13 }}>
                  {g.objective}
                  {g.ended_reason ? `（${g.ended_reason}）` : ""}
                </p>
              </div>
            );
          }

          if (item.kind === "dispatch") {
            const d = item.dispatch;
            const isSent = item.direction === "sent";
            return (
              <div key={item.id} className="plan-card" data-dispatch-id={d.id}>
                <div className="plan-head">
                  <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                    {isSent ? <ArrowUpRight size={13} /> : <ArrowDownLeft size={13} />}
                    <span>
                      {isSent
                        ? `会话派发 -> ${d.to_session}`
                        : `收到会话派发 <- ${d.from_session}`}
                    </span>
                  </span>
                  <span className="chip">
                    depth {d.depth} · {d.submit.outcome}
                  </span>
                </div>
                <p style={{ margin: "6px 0 0", fontSize: 12.5, opacity: 0.85 }}>
                  执行身份：{d.member_id}
                  {d.created_session ? " · 自动新建目标会话" : ""}
                </p>
              </div>
            );
          }

          if (item.kind === "error") {
            return (
              <div key={item.id} className="error-banner" role="alert">
                <AlertTriangle size={15} style={{ flexShrink: 0, marginTop: 2 }} />
                <div>{item.message}</div>
              </div>
            );
          }

          return (
            <article
              key={item.id}
              className="turn-assistant"
              data-turn-kind="assistant"
              data-completed={item.completed ? "true" : "false"}
            >
              <div className="turn-assistant-meta">
                <strong>{harnessId}</strong>
                <span>seq #{item.seq}</span>
                <span>{formatTime(item.ts)}</span>
                {item.completed && item.stopReason === "cancelled" && (
                  <span className="chip err">已中断</span>
                )}
                {item.completed && item.stopReason === "failed" && (
                  <span className="chip err">执行失败</span>
                )}
              </div>

              {item.plan.length > 0 && <PlanStrip entries={item.plan} />}

              {item.thinking && <ThinkingStrip text={item.thinking} />}

              {item.toolCalls.map((tool) => (
                <ToolCallCard key={tool.tool_call_id} tool={tool} />
              ))}

              {item.approvals.map((approval) => (
                <ApprovalCard
                  key={approval.id}
                  approval={approval}
                  onResolve={onResolveApproval}
                />
              ))}

              {item.turnError && (
                <div className="error-banner" role="alert">
                  <AlertTriangle size={14} style={{ flexShrink: 0, marginTop: 2 }} />
                  <span>{item.turnError}</span>
                </div>
              )}

              {(item.text || !item.completed) && (
                <MarkdownContent text={item.text} streaming={!item.completed} />
              )}
            </article>
          );
        })}
      </div>
    </div>
  );
}
