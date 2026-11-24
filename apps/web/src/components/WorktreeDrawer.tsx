import {
  FileCode2,
  FolderTree,
  GitCompare,
  RefreshCw,
  ShieldCheck,
  Trash2,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { OpenMirasimClient } from "../protocol/client";
import type {
  ApprovalRuleView,
  SessionId,
  WorktreeDiff,
  WorktreeEntry,
  WorktreeFileContent,
} from "../protocol/types";

interface WorktreeDrawerProps {
  open: boolean;
  sessionId: SessionId | null;
  client: OpenMirasimClient;
  canManageRules?: boolean;
  onClose: () => void;
}

type TabKey = "diff" | "files" | "rules";

const MAX_DIFF_LINES = 2500;
const MAX_VISIBLE_FILES = 500;

function changeBadge(kind: string): { label: string; cls: "ok" | "warn" | "err" | "" } {
  switch (kind) {
    case "added":
    case "untracked":
      return { label: kind === "added" ? "A" : "?", cls: "ok" };
    case "deleted":
      return { label: "D", cls: "err" };
    case "renamed":
      return { label: "R", cls: "warn" };
    default:
      return { label: "M", cls: "warn" };
  }
}

function renderUnifiedDiff(patch: string) {
  if (!patch.trim()) {
    return (
      <div style={{ color: "var(--text-muted)", fontSize: 12.5, padding: "12px 0" }}>
        工作区当前无未提交或相对基线的变更。
      </div>
    );
  }
  const allLines = patch.split("\n");
  const clipped = allLines.length > MAX_DIFF_LINES;
  const lines = clipped ? allLines.slice(0, MAX_DIFF_LINES) : allLines;
  return (
    <pre className="diff-pre">
      {lines.map((line, i) => {
        let cls = "diff-line";
        if (line.startsWith("+++") || line.startsWith("---")) {
          cls += " meta";
        } else if (line.startsWith("@@")) {
          cls += " hunk";
        } else if (line.startsWith("+")) {
          cls += " add";
        } else if (line.startsWith("-")) {
          cls += " del";
        }
        return (
          <div key={i} className={cls}>
            {line || " "}
          </div>
        );
      })}
      {clipped && (
        <div className="diff-line meta">
          {`… 已省略后续 ${allLines.length - MAX_DIFF_LINES} 行 diff（避免大补丁阻塞渲染）`}
        </div>
      )}
    </pre>
  );
}

export function WorktreeDrawer({
  open,
  sessionId,
  client,
  canManageRules = true,
  onClose,
}: WorktreeDrawerProps) {
  const [tab, setTab] = useState<TabKey>("diff");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [diff, setDiff] = useState<WorktreeDiff | null>(null);
  const [entries, setEntries] = useState<WorktreeEntry[]>([]);
  const [fileFilter, setFileFilter] = useState("");
  const [selectedFile, setSelectedFile] = useState<WorktreeFileContent | null>(null);
  const [rules, setRules] = useState<ApprovalRuleView[]>([]);
  const reqIdRef = useRef(0);

  const loadActiveTab = useCallback(async () => {
    if (!open || !sessionId) return;
    const reqId = ++reqIdRef.current;
    setLoading(true);
    setError(null);
    try {
      if (tab === "diff") {
        const res = await client.getWorktreeDiff(sessionId);
        if (reqId !== reqIdRef.current) return;
        setDiff(res);
      } else if (tab === "files") {
        const list = await client.listWorktreeFiles(sessionId);
        if (reqId !== reqIdRef.current) return;
        setEntries(list);
      } else {
        const list = await client.listApprovalRules(sessionId);
        if (reqId !== reqIdRef.current) return;
        setRules(list);
      }
    } catch (err) {
      if (reqId !== reqIdRef.current) return;
      setError(err instanceof Error ? err.message : "加载工作区信息失败");
    } finally {
      if (reqId === reqIdRef.current) {
        setLoading(false);
      }
    }
  }, [open, sessionId, tab, client]);

  useEffect(() => {
    void loadActiveTab();
  }, [loadActiveTab]);

  useEffect(() => {
    reqIdRef.current += 1;
    setSelectedFile(null);
    setDiff(null);
    setEntries([]);
    setRules([]);
    setError(null);
  }, [sessionId]);

  const handleOpenFile = async (relPath: string) => {
    if (!sessionId) return;
    const reqId = ++reqIdRef.current;
    setLoading(true);
    setError(null);
    try {
      const content = await client.readWorktreeFile(sessionId, relPath);
      if (reqId !== reqIdRef.current) return;
      setSelectedFile(content);
    } catch (err) {
      if (reqId !== reqIdRef.current) return;
      setError(err instanceof Error ? err.message : "读取文件失败");
    } finally {
      if (reqId === reqIdRef.current) {
        setLoading(false);
      }
    }
  };

  const handleClearRules = async () => {
    if (!sessionId || !canManageRules) return;
    const reqId = ++reqIdRef.current;
    setLoading(true);
    setError(null);
    try {
      await client.clearApprovalRules(sessionId);
      if (reqId !== reqIdRef.current) return;
      setRules([]);
    } catch (err) {
      if (reqId !== reqIdRef.current) return;
      setError(err instanceof Error ? err.message : "清空审批规则失败");
    } finally {
      if (reqId === reqIdRef.current) {
        setLoading(false);
      }
    }
  };

  const filteredEntries = useMemo(() => {
    const q = fileFilter.trim().toLowerCase();
    if (!q) return entries;
    return entries.filter((e) => e.path.toLowerCase().includes(q));
  }, [entries, fileFilter]);

  if (!open) return null;

  return (
    <aside
      className="inspector worktree-drawer"
      id="worktree-drawer"
      aria-label="工作区文件树、Git Diff 与审批规则"
    >
      <div className="inspector-head">
        <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
          <GitCompare size={14} />
          <span>Worktree & Diff</span>
        </span>
        <div style={{ display: "flex", gap: 4 }}>
          <button
            type="button"
            className="icon-btn"
            onClick={() => void loadActiveTab()}
            aria-label="刷新工作区"
            title="刷新"
          >
            <RefreshCw size={14} />
          </button>
          <button
            type="button"
            className="icon-btn"
            onClick={onClose}
            aria-label="关闭工作区面板"
          >
            <X size={15} />
          </button>
        </div>
      </div>

      <div className="drawer-tabs">
        <button
          type="button"
          id="tab-worktree-diff"
          className={`drawer-tab ${tab === "diff" ? "active" : ""}`}
          onClick={() => setTab("diff")}
        >
          <GitCompare size={13} />
          <span>Git Diff</span>
        </button>
        <button
          type="button"
          id="tab-worktree-files"
          className={`drawer-tab ${tab === "files" ? "active" : ""}`}
          onClick={() => setTab("files")}
        >
          <FolderTree size={13} />
          <span>文件树</span>
        </button>
        <button
          type="button"
          id="tab-approval-rules"
          className={`drawer-tab ${tab === "rules" ? "active" : ""}`}
          onClick={() => setTab("rules")}
        >
          <ShieldCheck size={13} />
          <span>审批规则</span>
        </button>
      </div>

      <div className="inspector-body">
        {error && (
          <div className="error-banner" role="alert" style={{ marginBottom: 10 }}>
            <span>{error}</span>
          </div>
        )}

        {loading && (
          <div style={{ fontSize: 12, color: "var(--text-muted)", padding: "8px 0" }}>
            加载中…
          </div>
        )}

        {tab === "diff" && diff && (
          <div className="drawer-section">
            <div className="drawer-meta-row">
              <span className="chip">base {diff.base_commit.slice(0, 8) || "HEAD"}</span>
              <span className="chip">{diff.files.length} 个变更文件</span>
              {diff.truncated && <span className="chip warn">已截断</span>}
            </div>
            {diff.files.length > 0 && (
              <ul className="worktree-file-list">
                {diff.files.map((f) => {
                  const b = changeBadge(f.kind);
                  return (
                    <li key={`${f.kind}:${f.path}`} className="worktree-file-item">
                      <span className={`chip ${b.cls}`}>{b.label}</span>
                      <span className="worktree-file-path" title={f.path}>
                        {f.old_path ? `${f.old_path} -> ${f.path}` : f.path}
                      </span>
                    </li>
                  );
                })}
              </ul>
            )}
            {renderUnifiedDiff(diff.patch)}
          </div>
        )}

        {tab === "files" && (
          <div className="drawer-section">
            <input
              id="worktree-file-search"
              type="search"
              className="input"
              placeholder="过滤文件路径…"
              value={fileFilter}
              onChange={(e) => setFileFilter(e.target.value)}
            />
            {selectedFile ? (
              <div className="file-preview">
                <div className="drawer-meta-row">
                  <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                    <FileCode2 size={13} />
                    <strong style={{ fontSize: 12 }}>{selectedFile.path}</strong>
                  </span>
                  <button
                    type="button"
                    className="btn btn-sm"
                    onClick={() => setSelectedFile(null)}
                  >
                    返回列表
                  </button>
                </div>
                {selectedFile.utf8 === false ? (
                  <div
                    style={{
                      color: "var(--text-muted)",
                      fontSize: 12.5,
                      padding: "12px 0",
                    }}
                  >
                    二进制文件（{selectedFile.size} B），不支持纯文本预览。
                  </div>
                ) : (
                  <pre className="diff-pre">{selectedFile.content}</pre>
                )}
              </div>
            ) : (
              <ul className="worktree-file-list">
                {filteredEntries.length === 0 ? (
                  <li style={{ color: "var(--text-muted)", fontSize: 12.5, padding: "8px 0" }}>
                    暂无文件
                  </li>
                ) : (
                  <>
                    {filteredEntries.slice(0, MAX_VISIBLE_FILES).map((entry) => (
                      <li key={entry.path}>
                        <button
                          type="button"
                          className="worktree-file-btn"
                          onClick={() => void handleOpenFile(entry.path)}
                        >
                          <span className="worktree-file-path">{entry.path}</span>
                          <span className="worktree-file-size">{entry.size} B</span>
                        </button>
                      </li>
                    ))}
                    {filteredEntries.length > MAX_VISIBLE_FILES && (
                      <li
                        style={{
                          color: "var(--text-muted)",
                          fontSize: 12,
                          padding: "8px 4px",
                        }}
                      >
                        仅展示前 {MAX_VISIBLE_FILES} / {filteredEntries.length} 个文件，请输入路径关键字过滤。
                      </li>
                    )}
                  </>
                )}
              </ul>
            )}
          </div>
        )}

        {tab === "rules" && (
          <div className="drawer-section">
            <div className="drawer-meta-row">
              <span className="chip">{rules.length} 条记忆规则</span>
              {canManageRules && rules.length > 0 && (
                <button
                  type="button"
                  id="clear-approval-rules-btn"
                  className="btn btn-danger btn-sm"
                  onClick={() => void handleClearRules()}
                >
                  <Trash2 size={12} />
                  <span>清空规则</span>
                </button>
              )}
            </div>
            {rules.length === 0 ? (
              <div style={{ color: "var(--text-muted)", fontSize: 12.5, padding: "8px 0" }}>
                当前会话暂无已记住的「始终允许 / 始终拒绝」审批规则。
              </div>
            ) : (
              <ul className="worktree-file-list">
                {rules.map((rule) => (
                  <li key={rule.rule_key} className="worktree-file-item">
                    <span className={`chip ${rule.allow ? "ok" : "err"}`}>
                      {rule.allow ? "允许" : "拒绝"}
                    </span>
                    <span className="worktree-file-path">{rule.rule_key}</span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
      </div>
    </aside>
  );
}
