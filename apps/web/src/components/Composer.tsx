import { Send, Square } from "lucide-react";
import { useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import type { SessionStatus } from "../protocol/types";

interface ComposerProps {
  status: SessionStatus;
  disabled: boolean;
  onSend: (text: string) => Promise<void>;
  onCancel: () => Promise<void>;
}

export function Composer({ status, disabled, onSend, onCancel }: ComposerProps) {
  const [text, setText] = useState("");
  const [sending, setSending] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  const isActive = status === "running" || status === "waiting_approval";

  const adjustHeight = () => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(180, Math.max(44, el.scrollHeight))}px`;
  };

  const submit = async () => {
    const trimmed = text.trim();
    if (!trimmed || sending || disabled || isActive) return;
    setSending(true);
    try {
      setText("");
      if (textareaRef.current) {
        textareaRef.current.style.height = "44px";
      }
      await onSend(trimmed);
    } finally {
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

  return (
    <div className="composer-wrap">
      <form className="composer-box" onSubmit={handleSubmit}>
        <textarea
          ref={textareaRef}
          id="prompt-textarea"
          className="composer-textarea"
          rows={2}
          value={text}
          disabled={disabled || isActive}
          placeholder={
            status === "waiting_approval"
              ? "当前回合等待审批，请在上方审批卡片中选择放行或拒绝…"
              : isActive
                ? "Agent 正在执行中…"
                : "输入给 Agent 的指令（Ctrl/Cmd + Enter 发送）…"
          }
          onChange={(e) => {
            setText(e.target.value);
            adjustHeight();
          }}
          onKeyDown={handleKeyDown}
        />

        <div className="composer-footer">
          <span className="composer-hint">Ctrl/Cmd + Enter 发送</span>
          <div style={{ display: "flex", gap: 8 }}>
            {isActive ? (
              <button
                id="cancel-turn-btn"
                type="button"
                className="btn btn-danger btn-sm"
                onClick={() => void onCancel()}
              >
                <Square size={12} />
                <span>中断回合</span>
              </button>
            ) : (
              <button
                id="send-prompt-btn"
                type="submit"
                className="btn btn-primary btn-sm"
                disabled={disabled || sending || !text.trim()}
              >
                <Send size={13} />
                <span>发送</span>
              </button>
            )}
          </div>
        </div>
      </form>
    </div>
  );
}
