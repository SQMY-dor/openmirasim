import {
  Activity,
  AlertCircle,
  Menu,
  Wifi,
  WifiOff,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Composer } from "./components/Composer";
import { ConnectView } from "./components/ConnectView";
import { EventInspector } from "./components/EventInspector";
import { NewSessionModal } from "./components/NewSessionModal";
import { SessionStream } from "./components/SessionStream";
import { sessionDisplayTitle, Sidebar } from "./components/Sidebar";
import type { OpenMirasimClient } from "./protocol/client";
import { HttpOpenMirasimClient } from "./protocol/httpClient";
import { MockOpenMirasimClient } from "./protocol/mockClient";
import {
  applySessionEvent,
  createEmptyProjection,
  type SessionProjection,
} from "./protocol/reducer";
import type {
  ApprovalDecision,
  ApprovalId,
  ConnectionState,
  CreateSessionRequest,
  ServerInfo,
  SessionEvent,
  SessionSummary,
} from "./protocol/types";

const AUTH_STORAGE_KEY = "openmirasim.auth.v1";
const THEME_STORAGE_KEY = "openmirasim.theme.v1";
const ACTIVE_SESSION_KEY = "openmirasim.active_session.v1";

interface SavedAuth {
  mode: "http" | "mock";
  serverUrl: string;
  token: string;
}

function readSavedAuth(): SavedAuth | null {
  try {
    const raw = window.localStorage.getItem(AUTH_STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as SavedAuth;
    if (parsed && (parsed.mode === "http" || parsed.mode === "mock")) {
      return parsed;
    }
  } catch {
    // ignore storage failures
  }
  return null;
}

function readSavedTheme(): "dark" | "light" {
  try {
    const saved = window.localStorage.getItem(THEME_STORAGE_KEY);
    if (saved === "light" || saved === "dark") return saved;
  } catch {
    // ignore
  }
  return "dark";
}

function connectionBadge(state: ConnectionState, lastSeq: number): {
  label: string;
  cls: "ok" | "warn" | "err" | "";
} {
  switch (state) {
    case "connected":
      return { label: `seq #${lastSeq} · 已同步`, cls: "ok" };
    case "catching_up":
      return { label: `seq #${lastSeq} · 补齐中…`, cls: "warn" };
    case "reconnecting":
      return { label: `seq #${lastSeq} · 重连中…`, cls: "warn" };
    case "error":
      return { label: "连接错误", cls: "err" };
    default:
      return { label: "未连接", cls: "" };
  }
}

export function App() {
  const [theme, setTheme] = useState<"dark" | "light">(readSavedTheme);
  const [client, setClient] = useState<OpenMirasimClient | null>(null);
  const [serverInfo, setServerInfo] = useState<ServerInfo | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [connectError, setConnectError] = useState<string | null>(null);

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(
    () => {
      try {
        return window.localStorage.getItem(ACTIVE_SESSION_KEY);
      } catch {
        return null;
      }
    }
  );
  const [projection, setProjection] = useState<SessionProjection | null>(null);
  const [connectionState, setConnectionState] =
    useState<ConnectionState>("disconnected");
  const [headSeq, setHeadSeq] = useState(0);
  const [streamError, setStreamError] = useState<string | null>(null);
  const [reconnectCount, setReconnectCount] = useState(0);
  const [resyncCount, setResyncCount] = useState(0);

  const [inspectorOpen, setInspectorOpen] = useState(false);
  const [newModalOpen, setNewModalOpen] = useState(false);
  const [creatingSession, setCreatingSession] = useState(false);
  const [mobileSidebarOpen, setMobileSidebarOpen] = useState(false);
  const [simulatingDrop, setSimulatingDrop] = useState(false);

  const lastSeqRef = useRef(0);
  useEffect(() => {
    lastSeqRef.current = projection?.lastSeq ?? 0;
  }, [projection]);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, theme);
    } catch {
      // ignore
    }
  }, [theme]);

  const toggleTheme = useCallback(() => {
    setTheme((prev) => (prev === "dark" ? "light" : "dark"));
  }, []);

  const bootstrapClient = useCallback(
    async (nextClient: OpenMirasimClient, authToPersist: SavedAuth) => {
      setConnecting(true);
      setConnectError(null);
      try {
        const info = await nextClient.connect();
        const sessionList = await nextClient.listSessions();
        try {
          window.localStorage.setItem(
            AUTH_STORAGE_KEY,
            JSON.stringify(authToPersist)
          );
        } catch {
          // ignore
        }
        setClient(nextClient);
        setServerInfo(info);
        setSessions(sessionList);
        setSelectedSessionId((prev) => {
          if (prev && sessionList.some((s) => s.id === prev)) return prev;
          return sessionList[0]?.id ?? null;
        });
      } catch (err) {
        setConnectError(
          err instanceof Error ? err.message : "无法连接到 OpenMirasim 中心服务器"
        );
      } finally {
        setConnecting(false);
      }
    },
    []
  );

  // Auto-restore saved connection on browser reload
  useEffect(() => {
    const saved = readSavedAuth();
    if (!saved) return;
    if (saved.mode === "mock") {
      void bootstrapClient(new MockOpenMirasimClient(), saved);
    } else if (saved.serverUrl && saved.token) {
      void bootstrapClient(
        new HttpOpenMirasimClient(saved.serverUrl, saved.token),
        saved
      );
    }
  }, [bootstrapClient]);

  const handleConnectServer = useCallback(
    async (serverUrl: string, token: string) => {
      const next = new HttpOpenMirasimClient(serverUrl, token);
      await bootstrapClient(next, { mode: "http", serverUrl, token });
    },
    [bootstrapClient]
  );

  const handleEnterDemo = useCallback(async () => {
    const next = new MockOpenMirasimClient();
    await bootstrapClient(next, {
      mode: "mock",
      serverUrl: next.serverUrl,
      token: "",
    });
  }, [bootstrapClient]);

  const handleDisconnect = useCallback(() => {
    try {
      window.localStorage.removeItem(AUTH_STORAGE_KEY);
    } catch {
      // ignore
    }
    setClient(null);
    setServerInfo(null);
    setSessions([]);
    setProjection(null);
    setConnectionState("disconnected");
    setStreamError(null);
    setReconnectCount(0);
    setResyncCount(0);
    setHeadSeq(0);
  }, []);

  const selectedSession = useMemo(
    () => sessions.find((s) => s.id === selectedSessionId) ?? null,
    [sessions, selectedSessionId]
  );

  useEffect(() => {
    try {
      if (selectedSessionId) {
        window.localStorage.setItem(ACTIVE_SESSION_KEY, selectedSessionId);
      }
    } catch {
      // ignore
    }
  }, [selectedSessionId]);

  // Subscribe to selected session with `Hello -> Welcome -> Subscribe { last_seq }`
  useEffect(() => {
    if (!client || !selectedSessionId) {
      setProjection(null);
      setConnectionState("disconnected");
      return;
    }

    const initialStatus =
      sessions.find((s) => s.id === selectedSessionId)?.status ?? "idle";
    lastSeqRef.current = 0;
    setProjection(createEmptyProjection(selectedSessionId, initialStatus));
    setReconnectCount(0);
    setResyncCount(0);
    setHeadSeq(0);
    setStreamError(null);

    const handleEvent = (record: SessionEvent) => {
      setProjection((prev) => {
        const base =
          prev && prev.sessionId === selectedSessionId
            ? prev
            : createEmptyProjection(selectedSessionId);
        const next = applySessionEvent(base, record);
        lastSeqRef.current = next.lastSeq;
        return next;
      });

      setHeadSeq((prev) => Math.max(prev, record.seq));

      setSessions((prev) =>
        prev.map((s) => {
          if (s.id !== record.session_id) return s;
          let status = s.status;
          const ev = record.payload;
          if (ev.type === "status_changed") status = ev.to;
          if (ev.type === "approval_requested") status = "waiting_approval";
          if (ev.type === "approval_resolved") status = "running";
          if (ev.type === "turn_ended") {
            status = ev.stop_reason === "failed" ? "failed" : "idle";
          }
          return {
            ...s,
            status,
            head_seq: Math.max(s.head_seq, record.seq),
            updated_at: record.ts,
          };
        })
      );
    };

    const sub = client.subscribeSession(
      selectedSessionId,
      () => lastSeqRef.current,
      {
        onEvent: handleEvent,
        onHeadSeq: (seq) => setHeadSeq((prev) => Math.max(prev, seq)),
        onStateChange: (state, err) => {
          setConnectionState(state);
          if (err !== undefined) {
            setStreamError(err);
          }
        },
        onReconnectAttempt: () => {
          setReconnectCount((c) => c + 1);
        },
        onResync: () => {
          setResyncCount((c) => c + 1);
        },
      }
    );

    return () => {
      sub.close();
    };
    // Re-subscribe only when client or selectedSessionId changes
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client, selectedSessionId]);

  const handleCreateSession = useCallback(
    async (req: CreateSessionRequest) => {
      if (!client) return;
      setCreatingSession(true);
      setStreamError(null);
      try {
        const created = await client.createSession(req);
        setSessions((prev) => [created, ...prev]);
        setSelectedSessionId(created.id);
      } catch (err) {
        setStreamError(err instanceof Error ? err.message : "创建会话失败");
      } finally {
        setCreatingSession(false);
      }
    },
    [client]
  );

  const handleSendPrompt = useCallback(
    async (text: string) => {
      if (!client || !selectedSessionId) return;
      setStreamError(null);
      try {
        await client.submitPrompt(selectedSessionId, text);
      } catch (err) {
        setStreamError(err instanceof Error ? err.message : "发送指令失败");
      }
    },
    [client, selectedSessionId]
  );

  const handleCancel = useCallback(async () => {
    if (!client || !selectedSessionId) return;
    try {
      await client.cancelSession(selectedSessionId);
    } catch (err) {
      setStreamError(err instanceof Error ? err.message : "中断回合失败");
    }
  }, [client, selectedSessionId]);

  const handleResolveApproval = useCallback(
    async (approvalId: ApprovalId, decision: ApprovalDecision) => {
      if (!client) return;
      try {
        await client.resolveApproval(approvalId, decision);
      } catch (err) {
        setStreamError(err instanceof Error ? err.message : "提交审批失败");
      }
    },
    [client]
  );

  const handleSimulateDrop = useCallback(async () => {
    if (!client?.simulateNetworkDrop || !selectedSessionId || simulatingDrop) {
      return;
    }
    setSimulatingDrop(true);
    try {
      await client.simulateNetworkDrop(selectedSessionId);
    } finally {
      setSimulatingDrop(false);
    }
  }, [client, selectedSessionId, simulatingDrop]);

  if (!client || !serverInfo) {
    const saved = readSavedAuth();
    const defaultUrl =
      saved?.mode === "http" && saved.serverUrl
        ? saved.serverUrl
        : window.location.port === "5173"
          ? "http://127.0.0.1:4939"
          : window.location.origin;

    return (
      <ConnectView
        initialServerUrl={defaultUrl}
        initialToken={saved?.mode === "http" ? saved.token : ""}
        theme={theme}
        connecting={connecting}
        error={connectError}
        onToggleTheme={toggleTheme}
        onConnectServer={handleConnectServer}
        onEnterDemo={handleEnterDemo}
      />
    );
  }

  const activeSeq = projection?.lastSeq ?? selectedSession?.head_seq ?? 0;
  const activeStatus = projection?.status ?? selectedSession?.status ?? "idle";
  const badge = connectionBadge(connectionState, activeSeq);

  return (
    <div className="workbench">
      <Sidebar
        serverInfo={serverInfo}
        serverUrl={client.serverUrl}
        mode={client.mode}
        sessions={sessions}
        selectedSessionId={selectedSessionId}
        theme={theme}
        mobileOpen={mobileSidebarOpen}
        onSelectSession={setSelectedSessionId}
        onNewSession={() => setNewModalOpen(true)}
        onToggleTheme={toggleTheme}
        onDisconnect={handleDisconnect}
        onCloseMobile={() => setMobileSidebarOpen(false)}
      />

      <main className="stage">
        <header className="stage-header">
          <div className="stage-header-left">
            <button
              type="button"
              className="icon-btn mobile-only"
              onClick={() => setMobileSidebarOpen(true)}
              aria-label="打开会话列表"
            >
              <Menu size={17} />
            </button>
            <div className="stage-title-group">
              <div className="stage-title" id="active-session-title">
                {selectedSession
                  ? sessionDisplayTitle(selectedSession)
                  : "请选择或新建会话"}
              </div>
              {selectedSession && (
                <div className="stage-cwd">
                  {selectedSession.config.harness_id} ·{" "}
                  {selectedSession.config.sandbox} · {selectedSession.config.cwd}
                </div>
              )}
            </div>
          </div>

          <div className="stage-header-right">
            {selectedSession && (
              <span className={`chip ${badge.cls}`} id="sync-status-chip">
                {connectionState === "connected" ? (
                  <Wifi size={12} />
                ) : (
                  <WifiOff size={12} />
                )}
                <span>{badge.label}</span>
              </span>
            )}

            {client.mode === "mock" && selectedSession && (
              <button
                type="button"
                id="simulate-drop-btn"
                className="btn btn-sm"
                disabled={simulatingDrop}
                onClick={() => void handleSimulateDrop()}
                title="模拟网络短暂中断并验证按 last_seq 自动追平"
              >
                {simulatingDrop ? "断网续传中…" : "模拟断网"}
              </button>
            )}

            <button
              type="button"
              id="toggle-inspector-btn"
              className={`icon-btn ${inspectorOpen ? "active" : ""}`}
              onClick={() => setInspectorOpen((v) => !v)}
              aria-label="切换事件流诊断面板"
              title="Event Stream Inspector"
            >
              <Activity size={16} />
            </button>
          </div>
        </header>

        {streamError && (
          <div style={{ padding: "10px 20px 0" }}>
            <div className="error-banner" role="alert" style={{ marginBottom: 0 }}>
              <AlertCircle size={15} style={{ flexShrink: 0, marginTop: 2 }} />
              <span>{streamError}</span>
            </div>
          </div>
        )}

        {selectedSession ? (
          <>
            <SessionStream
              harnessId={selectedSession.config.harness_id}
              items={projection?.items ?? []}
              onResolveApproval={handleResolveApproval}
            />
            <Composer
              status={activeStatus}
              disabled={
                connectionState === "disconnected" || activeStatus === "closed"
              }
              onSend={handleSendPrompt}
              onCancel={handleCancel}
            />
          </>
        ) : (
          <div className="empty-stage">
            <h2>欢迎来到 OpenMirasim</h2>
            <p>创建一个新的 Agent 会话开始工作，或从左侧列表恢复历史会话。</p>
            <button
              type="button"
              className="btn btn-primary"
              onClick={() => setNewModalOpen(true)}
            >
              新建会话
            </button>
          </div>
        )}
      </main>

      <EventInspector
        open={inspectorOpen}
        diagnostics={{
          connectionState,
          lastSeq: activeSeq,
          headSeq: Math.max(headSeq, activeSeq),
          reconnectCount,
          resyncCount,
          dedupedCount: projection?.dedupedCount ?? 0,
          gapDetectedCount: projection?.gapDetectedCount ?? 0,
          lastError: streamError,
        }}
        rawEvents={projection?.rawEvents ?? []}
        onClose={() => setInspectorOpen(false)}
      />

      <NewSessionModal
        open={newModalOpen}
        creating={creatingSession}
        onClose={() => setNewModalOpen(false)}
        onCreate={handleCreateSession}
      />
    </div>
  );
}
