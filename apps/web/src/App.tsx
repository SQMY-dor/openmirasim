import {
  Activity,
  AlertCircle,
  Bell,
  BellRing,
  GitCompare,
  Menu,
  Monitor,
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
import { TeamConsole } from "./components/TeamConsole";
import { WorktreeDrawer } from "./components/WorktreeDrawer";
import type { OpenMirasimClient } from "./protocol/client";
import { HttpOpenMirasimClient } from "./protocol/httpClient";
import { MockOpenMirasimClient } from "./protocol/mockClient";
import {
  applySessionEvent,
  createEmptyProjection,
  type SessionProjection,
} from "./protocol/reducer";
import {
  roleCan,
  type ApprovalDecision,
  type ApprovalId,
  type ConnectionState,
  type CreateSessionRequest,
  type HarnessDescriptor,
  type Principal,
  type QueuedPromptId,
  type RemoteScreenInfo,
  type ServerInfo,
  type SessionEvent,
  type SessionSummary,
  type SubmitMode,
  type WorkspaceSummary,
} from "./protocol/types";
import {
  enableApprovalPush,
  notifyPendingApproval,
  readPushPreference,
  registerServiceWorker,
  savePushPreference,
  subscribeServiceWorkerActions,
} from "./pwa/push";

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
  const [principal, setPrincipal] = useState<Principal | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [connectError, setConnectError] = useState<string | null>(null);

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [harnesses, setHarnesses] = useState<HarnessDescriptor[]>([]);
  const [workspaces, setWorkspaces] = useState<WorkspaceSummary[]>([]);
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(
    () => {
      try {
        const params = new URLSearchParams(window.location.search);
        const fromUrl = params.get("session");
        if (fromUrl) return fromUrl;
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
  const [worktreeOpen, setWorktreeOpen] = useState(false);
  const [teamConsoleOpen, setTeamConsoleOpen] = useState(false);
  const [newModalOpen, setNewModalOpen] = useState(false);
  const [creatingSession, setCreatingSession] = useState(false);
  const [mobileSidebarOpen, setMobileSidebarOpen] = useState(false);
  const [simulatingDrop, setSimulatingDrop] = useState(false);
  const [remoteScreen, setRemoteScreen] = useState<RemoteScreenInfo | null>(null);
  const [pushEnabled, setPushEnabled] = useState<boolean>(readPushPreference);

  const lastSeqRef = useRef(0);
  const liveConnectedRef = useRef(false);
  useEffect(() => {
    lastSeqRef.current = projection?.lastSeq ?? 0;
  }, [projection]);

  useEffect(() => {
    liveConnectedRef.current = connectionState === "connected";
  }, [connectionState]);

  useEffect(() => {
    void registerServiceWorker();
  }, []);

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
        const [me, sessionList, harnessList, workspaceList] = await Promise.all([
          nextClient.getMe().catch(() => null),
          nextClient.listSessions(),
          nextClient.listHarnesses().catch(() => []),
          nextClient.listWorkspaces().catch(() => []),
        ]);
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
        setPrincipal(me);
        setHarnesses(harnessList);
        setWorkspaces(workspaceList);
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
    setPrincipal(null);
    setSessions([]);
    setWorkspaces([]);
    setWorktreeOpen(false);
    setInspectorOpen(false);
    setTeamConsoleOpen(false);
    setRemoteScreen(null);
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

  // Handle PWA notification click actions via Service Worker postMessage or URL deep-link
  useEffect(() => {
    if (!client) return;
    const unsubscribe = subscribeServiceWorkerActions({
      onFocusSession: (sid) => {
        if (sid) setSelectedSessionId(sid);
      },
      onApprovalAction: ({ approvalId, sessionId, optionId }) => {
        if (sessionId) setSelectedSessionId(sessionId);
        void client
          .resolveApproval(approvalId, {
            outcome: "selected",
            option_id: optionId,
          })
          .catch((err) => {
            setStreamError(
              err instanceof Error ? err.message : "通过推送通知处理审批失败"
            );
          });
      },
    });

    try {
      const params = new URLSearchParams(window.location.search);
      const approveId = params.get("approve");
      const optionId = params.get("option");
      if (approveId && optionId) {
        void client
          .resolveApproval(approveId, {
            outcome: "selected",
            option_id: optionId,
          })
          .catch(() => {});
        params.delete("approve");
        params.delete("option");
        const nextQs = params.toString();
        window.history.replaceState(
          null,
          "",
          nextQs ? `/?${nextQs}` : window.location.pathname
        );
      }
    } catch {
      // ignore
    }

    return unsubscribe;
  }, [client]);

  // Fetch remote-screen takeover metadata when an `antigravity-gui` session is active
  const refreshRemoteScreen = useCallback(
    async (sid: string) => {
      if (!client) return;
      try {
        const res = await client.getRemoteScreen(sid);
        setRemoteScreen(res.available && res.screen ? res.screen : null);
      } catch {
        setRemoteScreen(null);
      }
    },
    [client]
  );

  useEffect(() => {
    if (!client || !selectedSessionId) {
      setRemoteScreen(null);
      return;
    }
    void refreshRemoteScreen(selectedSessionId);
  }, [client, selectedSessionId, refreshRemoteScreen]);

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

      if (
        record.payload.type === "harness_ready" &&
        record.payload.capabilities.remote_screen
      ) {
        void refreshRemoteScreen(record.session_id);
      }

      if (
        record.payload.type === "approval_requested" &&
        liveConnectedRef.current
      ) {
        void notifyPendingApproval(record.session_id, record.payload.approval);
      }

      setSessions((prev) =>
        prev.map((s) => {
          if (s.id !== record.session_id) return s;
          const ev = record.payload;
          const status = ev.type === "status_changed" ? ev.to : s.status;
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

  const handleRegisterWorkspace = useCallback(
    async (name: string, repoPath: string): Promise<WorkspaceSummary> => {
      if (!client) {
        throw new Error("未连接到服务器");
      }
      const created = await client.registerWorkspace(name, repoPath);
      setWorkspaces((prev) => {
        const filtered = prev.filter((w) => w.id !== created.id);
        return [created, ...filtered];
      });
      return created;
    },
    [client]
  );

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
    async (text: string, mode?: SubmitMode) => {
      if (!client || !selectedSessionId) return;
      setStreamError(null);
      try {
        await client.submitPrompt(selectedSessionId, text, mode);
      } catch (err) {
        setStreamError(err instanceof Error ? err.message : "发送指令失败");
      }
    },
    [client, selectedSessionId]
  );

  const handleWithdrawQueued = useCallback(
    async (queuedId: QueuedPromptId) => {
      if (!client || !selectedSessionId) return;
      setStreamError(null);
      try {
        await client.withdrawQueued(selectedSessionId, queuedId);
      } catch (err) {
        setStreamError(err instanceof Error ? err.message : "撤回排队指令失败");
      }
    },
    [client, selectedSessionId]
  );

  const handleMoveQueued = useCallback(
    async (queuedId: QueuedPromptId, position: number) => {
      if (!client || !selectedSessionId) return;
      setStreamError(null);
      try {
        await client.moveQueued(selectedSessionId, queuedId, position);
      } catch (err) {
        setStreamError(err instanceof Error ? err.message : "调整队列顺序失败");
      }
    },
    [client, selectedSessionId]
  );

  const handleResumeQueue = useCallback(async () => {
    if (!client || !selectedSessionId) return;
    setStreamError(null);
    try {
      await client.resumeQueue(selectedSessionId);
    } catch (err) {
      setStreamError(err instanceof Error ? err.message : "恢复队列失败");
    }
  }, [client, selectedSessionId]);

  const handleCancel = useCallback(async () => {
    if (!client || !selectedSessionId) return;
    try {
      await client.cancelSession(selectedSessionId);
    } catch (err) {
      setStreamError(err instanceof Error ? err.message : "中断回合失败");
    }
  }, [client, selectedSessionId]);

  const handleResume = useCallback(async () => {
    if (!client || !selectedSessionId) return;
    setStreamError(null);
    try {
      await client.resumeSession(selectedSessionId);
    } catch (err) {
      setStreamError(err instanceof Error ? err.message : "恢复会话失败");
    }
  }, [client, selectedSessionId]);

  const handleCloseSession = useCallback(async () => {
    if (!client || !selectedSessionId) return;
    setStreamError(null);
    try {
      await client.closeSession(selectedSessionId);
      setRemoteScreen(null);
    } catch (err) {
      setStreamError(err instanceof Error ? err.message : "关闭会话失败");
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

  const handleTogglePush = useCallback(async () => {
    if (!client) return;
    if (pushEnabled) {
      savePushPreference(false);
      setPushEnabled(false);
      return;
    }
    const res = await enableApprovalPush(client);
    setPushEnabled(res.ok);
    if (!res.ok) {
      setStreamError(res.detail);
    }
  }, [client, pushEnabled]);

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
  const canManageSession =
    !principal || roleCan(principal.role, "create_session");
  const canPrompt = !principal || roleCan(principal.role, "prompt_session");

  return (
    <div className="workbench">
      <Sidebar
        serverInfo={serverInfo}
        serverUrl={client.serverUrl}
        mode={client.mode}
        principal={principal}
        sessions={sessions}
        selectedSessionId={selectedSessionId}
        theme={theme}
        mobileOpen={mobileSidebarOpen}
        onSelectSession={setSelectedSessionId}
        onNewSession={() => setNewModalOpen(true)}
        onOpenTeamConsole={() => setTeamConsoleOpen(true)}
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
            {projection && projection.queue.length > 0 && (
              <span className="chip warn" id="queue-count-chip">
                队列 {projection.queue.length}
                {projection.queuePaused ? "（已暂停）" : ""}
              </span>
            )}

            {projection?.goal && projection.goal.status === "active" && (
              <span className="chip warn" id="active-goal-chip">
                目标 {projection.goal.turns_used}/{projection.goal.max_turns}
              </span>
            )}

            {remoteScreen && remoteScreen.available && (
              <a
                id="open-remote-screen-btn"
                className="btn btn-primary btn-sm"
                href={remoteScreen.takeover_url}
                target="_blank"
                rel="noopener noreferrer"
                title={`隔离显示 ${remoteScreen.display} (${remoteScreen.driver})`}
              >
                <Monitor size={13} />
                <span>接管 GUI (noVNC)</span>
              </a>
            )}

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

            {selectedSession && activeStatus === "failed" && canManageSession && (
              <button
                type="button"
                id="resume-session-btn"
                className="btn btn-primary btn-sm"
                onClick={() => void handleResume()}
              >
                恢复会话
              </button>
            )}

            {selectedSession && activeStatus !== "closed" && canManageSession && (
              <button
                type="button"
                id="close-session-btn"
                className="btn btn-sm"
                onClick={() => void handleCloseSession()}
                title="永久关闭此会话并释放 Agent 进程"
              >
                关闭会话
              </button>
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
              id="toggle-push-btn"
              className={`icon-btn ${pushEnabled ? "active" : ""}`}
              onClick={() => void handleTogglePush()}
              aria-label="切换 PWA 审批推送提醒"
              title={
                pushEnabled
                  ? "PWA 审批推送提醒已开启（点击关闭）"
                  : "开启 PWA 审批推送提醒"
              }
            >
              {pushEnabled ? <BellRing size={16} /> : <Bell size={16} />}
            </button>

            {selectedSession && (
              <button
                type="button"
                id="toggle-worktree-btn"
                className={`icon-btn ${worktreeOpen ? "active" : ""}`}
                onClick={() => {
                  setWorktreeOpen((v) => {
                    const next = !v;
                    if (next) setInspectorOpen(false);
                    return next;
                  });
                }}
                aria-label="切换 Worktree Diff / 文件树 / 审批规则面板"
                title="Worktree Diff · Files · Approval Rules"
              >
                <GitCompare size={16} />
              </button>
            )}

            <button
              type="button"
              id="toggle-inspector-btn"
              className={`icon-btn ${inspectorOpen ? "active" : ""}`}
              onClick={() => {
                setInspectorOpen((v) => {
                  const next = !v;
                  if (next) setWorktreeOpen(false);
                  return next;
                });
              }}
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
            {canPrompt ? (
              <Composer
                status={activeStatus}
                disabled={
                  connectionState === "disconnected" || activeStatus === "closed"
                }
                queue={projection?.queue ?? []}
                queuePaused={projection?.queuePaused ?? false}
                onSend={handleSendPrompt}
                onCancel={handleCancel}
                onWithdrawQueued={handleWithdrawQueued}
                onMoveQueued={handleMoveQueued}
                onResumeQueue={handleResumeQueue}
              />
            ) : (
              <div className="composer-wrap">
                <div className="viewer-composer-banner" id="viewer-composer-banner">
                  当前角色为 <strong>Viewer（只读观察者）</strong>
                  ：可实时同步查看会话流式输出、文件树与 Git Diff，无法发送指令或操作审批。
                </div>
              </div>
            )}
          </>
        ) : (
          <div className="empty-stage">
            <h2>欢迎来到 OpenMirasim</h2>
            <p>
              {canManageSession
                ? "创建一个新的 Agent 会话开始工作，或从左侧列表恢复历史会话。"
                : "从左侧列表选择任意会话以只读模式实时观摩协作流。"}
            </p>
            {canManageSession && (
              <button
                type="button"
                className="btn btn-primary"
                onClick={() => setNewModalOpen(true)}
              >
                新建会话
              </button>
            )}
          </div>
        )}
      </main>

      <WorktreeDrawer
        open={worktreeOpen}
        sessionId={selectedSessionId}
        client={client}
        onClose={() => setWorktreeOpen(false)}
      />

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
        harnesses={harnesses}
        workspaces={workspaces}
        onRegisterWorkspace={handleRegisterWorkspace}
        onClose={() => setNewModalOpen(false)}
        onCreate={handleCreateSession}
      />

      <TeamConsole
        open={teamConsoleOpen}
        client={client}
        principal={principal}
        onClose={() => setTeamConsoleOpen(false)}
        onPushStateChanged={setPushEnabled}
      />
    </div>
  );
}
