import { X } from "lucide-react";
import { useEffect, useMemo, useState, type FormEvent } from "react";
import {
  describeHarness,
  KNOWN_HARNESSES,
  type CreateSessionRequest,
  type HarnessDescriptor,
  type SandboxMode,
} from "../protocol/types";

interface NewSessionModalProps {
  open: boolean;
  creating: boolean;
  harnesses: HarnessDescriptor[];
  onClose: () => void;
  onCreate: (req: CreateSessionRequest) => Promise<void>;
}

export function NewSessionModal({
  open,
  creating,
  harnesses,
  onClose,
  onCreate,
}: NewSessionModalProps) {
  const catalog = useMemo(
    () =>
      harnesses.length > 0
        ? harnesses.map(describeHarness)
        : [...KNOWN_HARNESSES],
    [harnesses]
  );

  const [title, setTitle] = useState("");
  const [harnessId, setHarnessId] = useState(() => catalog[0]?.id ?? "mock");
  const [cwd, setCwd] = useState("/srv/workspaces/openmirasim");
  const [sandbox, setSandbox] = useState<SandboxMode>("direct");

  useEffect(() => {
    if (!catalog.some((h) => h.id === harnessId) && catalog.length > 0) {
      setHarnessId(catalog[0].id);
    }
  }, [catalog, harnessId]);

  if (!open) return null;

  const selectedHarness = catalog.find((h) => h.id === harnessId);

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    await onCreate({
      config: {
        harness_id: harnessId,
        cwd: cwd.trim() || "/srv/workspaces/openmirasim",
        sandbox,
        ...(title.trim() ? { title: title.trim() } : {}),
      },
    });
    setTitle("");
    onClose();
  };

  return (
    <div
      className="modal-backdrop"
      role="dialog"
      aria-modal="true"
      aria-labelledby="new-session-modal-title"
      onClick={onClose}
    >
      <div className="modal-card" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h2 id="new-session-modal-title">新建 Agent 会话</h2>
          <button
            type="button"
            className="icon-btn"
            onClick={onClose}
            aria-label="关闭弹窗"
          >
            <X size={16} />
          </button>
        </div>

        <form onSubmit={(e) => void handleSubmit(e)}>
          <div className="field">
            <label htmlFor="new-session-title">会话标题（可选）</label>
            <input
              id="new-session-title"
              className="input"
              type="text"
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              placeholder="例如：修复 SQLite 事件补齐逻辑"
              autoFocus
            />
          </div>

          <div className="field">
            <label htmlFor="new-session-harness">Agent Harness</label>
            <select
              id="new-session-harness"
              className="select"
              value={harnessId}
              onChange={(e) => setHarnessId(e.target.value)}
            >
              {catalog.map((h) => (
                <option key={h.id} value={h.id}>
                  {h.label}
                </option>
              ))}
            </select>
            {selectedHarness && (
              <div style={{ display: "flex", flexWrap: "wrap", gap: 6, marginTop: 4 }}>
                <span className="chip">{selectedHarness.protocol.toUpperCase()}</span>
                {selectedHarness.capabilities.native_approval && (
                  <span className="chip ok">原生审批</span>
                )}
                {selectedHarness.capabilities.steer && (
                  <span className="chip">Steer</span>
                )}
                {selectedHarness.capabilities.resume && (
                  <span className="chip">Resume</span>
                )}
              </div>
            )}
          </div>

          <div className="field">
            <label htmlFor="new-session-sandbox">执行隔离模式</label>
            <select
              id="new-session-sandbox"
              className="select"
              value={sandbox}
              onChange={(e) => setSandbox(e.target.value as SandboxMode)}
            >
              <option value="direct">direct（单人回环开发模式）</option>
              <option value="bwrap">bwrap（Linux bubblewrap 沙箱）</option>
            </select>
          </div>

          <div className="field">
            <label htmlFor="new-session-cwd">服务端工作区目录（cwd）</label>
            <input
              id="new-session-cwd"
              className="input input-mono"
              type="text"
              value={cwd}
              onChange={(e) => setCwd(e.target.value)}
              required
            />
          </div>

          <div className="modal-actions">
            <button type="button" className="btn" onClick={onClose}>
              取消
            </button>
            <button
              id="confirm-create-session-btn"
              type="submit"
              className="btn btn-primary"
              disabled={creating}
            >
              {creating ? "创建中…" : "创建会话"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
