import {
  LogOut,
  Moon,
  Plus,
  Search,
  Shield,
  Sun,
  TerminalSquare,
} from "lucide-react";
import { useMemo, useState } from "react";
import {
  roleCan,
  type Principal,
  type ServerInfo,
  type SessionSummary,
} from "../protocol/types";

interface SidebarProps {
  serverInfo: ServerInfo;
  serverUrl: string;
  mode: "http" | "mock";
  principal: Principal | null;
  sessions: SessionSummary[];
  selectedSessionId: string | null;
  theme: "dark" | "light";
  mobileOpen: boolean;
  onSelectSession: (id: string) => void;
  onNewSession: () => void;
  onOpenTeamConsole: () => void;
  onToggleTheme: () => void;
  onDisconnect: () => void;
  onCloseMobile: () => void;
}

function statusLabel(status: SessionSummary["status"]): string {
  switch (status) {
    case "starting":
      return "启动中";
    case "running":
      return "运行中";
    case "waiting_approval":
      return "待审批";
    case "failed":
      return "异常";
    case "closed":
      return "已关闭";
    default:
      return "空闲";
  }
}

export function sessionDisplayTitle(session: SessionSummary): string {
  return (
    session.config.title?.trim() ||
    `${session.config.harness_id} · ${session.config.cwd.split("/").pop() || session.id}`
  );
}

export function Sidebar({
  serverInfo,
  serverUrl,
  mode,
  principal,
  sessions,
  selectedSessionId,
  theme,
  mobileOpen,
  onSelectSession,
  onNewSession,
  onOpenTeamConsole,
  onToggleTheme,
  onDisconnect,
  onCloseMobile,
}: SidebarProps) {
  const [query, setQuery] = useState("");
  const canCreate = !principal || roleCan(principal.role, "create_session");

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return sessions;
    return sessions.filter(
      (s) =>
        sessionDisplayTitle(s).toLowerCase().includes(q) ||
        s.config.harness_id.toLowerCase().includes(q) ||
        s.config.cwd.toLowerCase().includes(q)
    );
  }, [sessions, query]);

  return (
    <>
      <div
        className={`sidebar-scrim ${mobileOpen ? "open" : ""}`}
        onClick={onCloseMobile}
        aria-hidden="true"
      />
      <aside className={`sidebar ${mobileOpen ? "open" : ""}`} aria-label="会话列表">
        <div className="sidebar-header">
          <div className="space-meta">
            <h1 className="space-name" title={serverInfo.spaceLabel}>
              {serverInfo.spaceLabel}
            </h1>
            <div className="space-url" title={serverUrl}>
              {serverUrl}
            </div>
          </div>
          <div style={{ display: "flex", gap: 2 }}>
            <button
              type="button"
              id="workbench-theme-toggle"
              className="icon-btn"
              onClick={onToggleTheme}
              aria-label={theme === "dark" ? "切换到浅色主题" : "切换到深色主题"}
            >
              {theme === "dark" ? <Sun size={15} /> : <Moon size={15} />}
            </button>
            <button
              type="button"
              id="disconnect-btn"
              className="icon-btn"
              onClick={onDisconnect}
              aria-label="断开连接"
              title="断开并切换服务器"
            >
              <LogOut size={15} />
            </button>
          </div>
        </div>

        {principal && (
          <div className="sidebar-identity" id="sidebar-principal">
            <div className="sidebar-identity-info">
              <span className="sidebar-identity-name" title={principal.display_name}>
                {principal.display_name}
              </span>
              <span
                className={`chip ${principal.role === "owner" ? "ok" : principal.role === "viewer" ? "warn" : ""}`}
                id="principal-role-badge"
              >
                {principal.role.toUpperCase()}
              </span>
            </div>
            <button
              type="button"
              id="open-team-console-btn"
              className="btn btn-sm"
              onClick={onOpenTeamConsole}
              title="成员管理、设备 Token、KeyProxy 与审计日志"
            >
              <Shield size={12} />
              <span>团队与安全</span>
            </button>
          </div>
        )}

        <div className="sidebar-controls">
          {canCreate ? (
            <button
              type="button"
              id="new-session-btn"
              className="btn btn-primary"
              onClick={onNewSession}
            >
              <Plus size={15} />
              <span>新建会话</span>
            </button>
          ) : (
            <div className="viewer-readonly-hint" id="viewer-readonly-hint">
              只读观察者模式（Viewer）
            </div>
          )}

          <div className="search-wrap">
            <Search size={13} />
            <input
              id="session-search-input"
              className="input search-input"
              type="search"
              placeholder="搜索会话 / Agent / 路径…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
          </div>
        </div>

        <nav className="session-list" id="session-list">
          {filtered.length === 0 ? (
            <div
              style={{
                padding: "24px 12px",
                textAlign: "center",
                color: "var(--text-muted)",
                fontSize: 12.5,
              }}
            >
              暂无匹配会话
            </div>
          ) : (
            filtered.map((session) => {
              const selected = session.id === selectedSessionId;
              return (
                <button
                  key={session.id}
                  type="button"
                  data-session-id={session.id}
                  className={`session-item ${selected ? "selected" : ""}`}
                  onClick={() => {
                    onSelectSession(session.id);
                    onCloseMobile();
                  }}
                >
                  <div className="session-item-top">
                    <span className="session-item-title">
                      {sessionDisplayTitle(session)}
                    </span>
                    <span
                      className={`status-dot ${session.status}`}
                      title={statusLabel(session.status)}
                    />
                  </div>
                  <div className="session-item-meta">
                    <span>{session.config.harness_id}</span>
                    <span>seq #{session.head_seq}</span>
                  </div>
                </button>
              );
            })
          )}
        </nav>

        <div className="sidebar-footer">
          <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
            <TerminalSquare size={14} />
            <span>{mode === "mock" ? "Local Mock" : "om-server"}</span>
          </span>
          <span className="chip">v{serverInfo.serverVersion}</span>
        </div>
      </aside>
    </>
  );
}
