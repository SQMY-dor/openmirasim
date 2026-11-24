import { ArrowUpToLine, Play, Send, Square, X } from "lucide-react";
import { useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import {
  renderContentBlocks,
  type QueuedPrompt,
  type QueuedPromptId,
  type SessionStatus,
  type SubmitMode,
} from "../protocol/types";

interface ComposerProps {
  status: SessionStatus;
  disabled: boolean;
  queue: QueuedPrompt[];
  queuePaused: boolean;
  onSend: (text: string, mode?: SubmitMode) => Promise<boolean>;
  onCancel: () => Promise<void>;
  onWithdrawQueued: (queuedId: QueuedPromptId) => Promise<void>;
  onMoveQueued: (queuedId: QueuedPromptId, position: number) => Promise<void>;
  onResumeQueue: () => Promise<void>;
}

export function Composer({
  status,
  disabled,
  queue,
  queuePaused,
  onSend,
  onCancel,
  onWithdrawQueued,
  onMoveQueued,
  onResumeQueue,
}: ComposerProps) {
  const [text, setText] = useState("");
  const [mode, setMode] = useState<SubmitMode>("queue");
  const [sending, setSending] = useState(false);
  const sendingRef = useRef(false);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  const isActive = status === "running" || status === "waiting_approval";
  // When prompts are queued (even if status is idle with a paused queue), we must submit with
  // an explicit queue mode so `om-server` routes through `Orchestrator::send_prompt` instead of
  // rejecting with 400.
  const useQueueMode = isActive || queue.length > 0;
  const cannotSend =
    disabled ||
    status === "starting" ||
    status === "failed" ||
    status === "closed";

  const adjustHeight = () => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(180, Math.max(44, el.scrollHeight))}px`;
  };

  const submit = async () => {
    const trimmed = text.trim();
    if (!trimmed || sendingRef.current || cannotSend) return;
    sendingRef.current = true;
    setSending(true);
    try {
      const ok = await onSend(trimmed, useQueueMode ? mode : undefined);
      if (ok) {
        setText("");
        if (textareaRef.current) {
          textareaRef.current.style.height = "44px";
        }
      }
    } finally {
      sendingRef.current = false;
      setSending(false);
    }
  };

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault();
    void submit();
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault();
      void submit();
    }
  };

  const placeholder =
    status === "starting"
      ? "Agent 正在启动中，请稍候…"
      : status === "failed"
        ? "会话已中断，请点击顶部「恢复会话」重新启动 Agent…"
        : status === "closed"
          ? "会话已关闭"
          : status === "waiting_approval"
            ? "等待审批中，也可直接输入后续指令加入队列（Queue / Steer）…"
            : isActive
              ? "Agent 正在执行中，可输入指令排队或 Steer 转向…"
              : "输入给 Agent 的指令（Ctrl/Cmd + Enter 发送）…";

  return (
    <div className="composer-wrap">
      {(queue.length > 0 || queuePaused) && (
        <div className="queue-strip" id="prompt-queue-strip">
          <div className="queue-strip-head">
            <span>
              待执行队列（{queue.length}）{queuePaused ? " · 已暂停" : ""}
            </span>
            {queuePaused && (
              <button
                type="button"
                id="resume-queue-btn"
                className="btn btn-primary btn-sm"
                onClick={() => void onResumeQueue()}
              >
                <Play size={11} />
                <span>恢复队列</span>
              </button>
            )}
          </div>
          {queue.map((item, idx) => (
            <div key={item.id} className="queue-item" data-queued-id={item.id}>
              <span className="queue-item-text">
                <span className="chip">#{idx + 1}</span>
                <span>{renderContentBlocks(item.prompt.content)}</span>
              </span>
              <span style={{ display: "inline-flex", gap: 4 }}>
                {idx > 0 && (
                  <button
                    type="button"
                    className="icon-btn"
                    title="置顶优先执行"
                    onClick={() => void onMoveQueued(item.id, 0)}
                  >
                    <ArrowUpToLine size={12} />
                  </button>
                )}
                <button
                  type="button"
                  className="icon-btn"
                  title="撤回"
                  onClick={() => void onWithdrawQueued(item.id)}
                >
                  <X size={12} />
                </button>
              </span>
            </div>
          ))}
        </div>
      )}

      <form className="composer-box" onSubmit={handleSubmit}>
        <textarea
          ref={textareaRef}
          id="prompt-textarea"
          className="composer-textarea"
          rows={2}
          value={text}
          disabled={cannotSend}
          placeholder={placeholder}
          onChange={(e) => {
            setText(e.target.value);
            adjustHeight();
          }}
          onKeyDown={handleKeyDown}
        />

        <div className="composer-footer">
          <span className="composer-hint">Ctrl/Cmd + Enter 发送</span>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            {useQueueMode && (
              <select
                id="submit-mode-select"
                className="select select-compact"
                value={mode}
                onChange={(e) => setMode(e.target.value as SubmitMode)}
                aria-label="运行中提交模式"
              >
                <option value="queue">排队 (Queue)</option>
                <option value="next">插队 (Next)</option>
                {isActive && <option value="steer">转向 (Steer)</option>}
              </select>
            )}

            {isActive && (
              <button
                id="cancel-turn-btn"
                type="button"
                className="btn btn-danger btn-sm"
                onClick={() => void onCancel()}
              >
                <Square size={12} />
                <span>中断回合</span>
              </button>
            )}

            <button
              id="send-prompt-btn"
              type="submit"
              className="btn btn-primary btn-sm"
              disabled={cannotSend || sending || !text.trim()}
            >
              <Send size={13} />
              <span>{isActive ? (mode === "steer" ? "Steer" : "入队") : "发送"}</span>
            </button>
          </div>
        </div>
      </form>
    </div>
  );
}
