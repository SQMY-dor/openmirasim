import { FolderGit2, Plus, X } from "lucide-react";
import { useEffect, useMemo, useState, type FormEvent } from "react";
import {
  describeHarness,
  KNOWN_HARNESSES,
  type CreateSessionRequest,
  type HarnessDescriptor,
  type SandboxMode,
  type WorkspaceSummary,
} from "../protocol/types";

interface NewSessionModalProps {
  open: boolean;
  creating: boolean;
  harnesses: HarnessDescriptor[];
  workspaces: WorkspaceSummary[];
  canManageWorkspaces?: boolean;
  onRegisterWorkspace: (name: string, repoPath: string) => Promise<WorkspaceSummary>;
  onClose: () => void;
  onCreate: (req: CreateSessionRequest) => Promise<boolean>;
}

export function NewSessionModal({
  open,
  creating,
  harnesses,
  workspaces,
  canManageWorkspaces = true,
  onRegisterWorkspace,
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
  const [workspaceId, setWorkspaceId] = useState<string>("");
  const [cwd, setCwd] = useState("/srv/workspaces/openmirasim");
  const [sandbox, setSandbox] = useState<SandboxMode>("direct");

  const [addingWs, setAddingWs] = useState(false);
  const [newWsName, setNewWsName] = useState("");
  const [newWsPath, setNewWsPath] = useState("");
  const [wsError, setWsError] = useState<string | null>(null);
  const [createError, setCreateError] = useState<string | null>(null);
  const [registeringWs, setRegisteringWs] = useState(false);

  useEffect(() => {
    if (!catalog.some((h) => h.id === harnessId) && catalog.length > 0) {
      setHarnessId(catalog[0].id);
    }
  }, [catalog, harnessId]);

  useEffect(() => {
    if (!open) return;
    setCreateError(null);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;

  const selectedHarness = catalog.find((h) => h.id === harnessId);

  const handleRegisterWorkspace = async () => {
    if (!newWsName.trim() || !newWsPath.trim()) return;
    setRegisteringWs(true);
    setWsError(null);
    try {
      const created = await onRegisterWorkspace(
        newWsName.trim(),
        newWsPath.trim()
      );
      setWorkspaceId(created.id);
      setNewWsName("");
      setNewWsPath("");
      setAddingWs(false);
    } catch (err) {
      setWsError(err instanceof Error ? err.message : "注册 Git 工作区失败");
    } finally {
      setRegisteringWs(false);
    }
  };

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setCreateError(null);
    const selectedWs = workspaces.find((w) => w.id === workspaceId);
    const ok = await onCreate({
      config: {
        harness_id: harnessId,
        cwd: selectedWs?.repo_path ?? (cwd.trim() || "/srv/workspaces/openmirasim"),
        sandbox,
        ...(workspaceId ? { workspace_id: workspaceId } : {}),
        ...(title.trim() ? { title: title.trim() } : {}),
      },
    });
    if (ok) {
      setTitle("");
      onClose();
    } else {
      setCreateError("创建会话失败，请检查顶部错误提示后重试");
    }
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
          {createError && (
            <div className="error-banner" role="alert" style={{ marginBottom: 10 }}>
              <span>{createError}</span>
            </div>
          )}
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
                {selectedHarness.capabilities.remote_screen && (
                  <span className="chip warn">noVNC 远程接管</span>
                )}
              </div>
            )}
          </div>

          <div className="field">
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}>
              <label htmlFor="new-session-workspace">Git 工作区（自动隔离 Worktree）</label>
              {canManageWorkspaces && (
                <button
                  type="button"
                  id="toggle-register-workspace-btn"
                  className="btn btn-ghost btn-sm"
                  onClick={() => {
                    setAddingWs((v) => !v);
                    setWsError(null);
                  }}
                >
                  <Plus size={12} />
                  <span>{addingWs ? "收起" : "注册仓库"}</span>
                </button>
              )}
            </div>
            <select
              id="new-session-workspace"
              className="select"
              value={workspaceId}
              onChange={(e) => setWorkspaceId(e.target.value)}
            >
              <option value="">不绑定工作区（使用下方自定义 cwd）</option>
              {workspaces.map((ws) => (
                <option key={ws.id} value={ws.id}>
                  {ws.name} ({ws.default_branch}) — {ws.repo_path}
                </option>
              ))}
            </select>

            {addingWs && (
              <div className="inline-register-box">
                <div style={{ display: "flex", alignItems: "center", gap: 6, fontSize: 12, marginBottom: 6 }}>
                  <FolderGit2 size={13} />
                  <span>注册宿主机 Git 仓库</span>
                </div>
                {wsError && (
                  <div className="error-banner" style={{ marginBottom: 6 }}>
                    <span>{wsError}</span>
                  </div>
                )}
                <input
                  id="register-ws-name"
                  className="input"
                  type="text"
                  placeholder="工作区名称（如 openmirasim）"
                  value={newWsName}
                  onChange={(e) => setNewWsName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      void handleRegisterWorkspace();
                    }
                  }}
                  style={{ marginBottom: 6 }}
                />
                <input
                  id="register-ws-path"
                  className="input input-mono"
                  type="text"
                  placeholder="宿主机仓库绝对路径（如 /srv/repos/app）"
                  value={newWsPath}
                  onChange={(e) => setNewWsPath(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      void handleRegisterWorkspace();
                    }
                  }}
                  style={{ marginBottom: 8 }}
                />
                <button
                  type="button"
                  id="confirm-register-ws-btn"
                  className="btn btn-sm btn-primary"
                  disabled={registeringWs || !newWsName.trim() || !newWsPath.trim()}
                  onClick={() => void handleRegisterWorkspace()}
                >
                  {registeringWs ? "注册中…" : "确认注册"}
                </button>
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

          {!workspaceId && (
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
          )}

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
