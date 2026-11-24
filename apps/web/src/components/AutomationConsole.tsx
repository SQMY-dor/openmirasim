import {
  CalendarClock,
  Pause,
  Play,
  RefreshCw,
  Send,
  Square,
  Target,
  Trash2,
  X,
  Zap,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import type { OpenMirasimClient } from "../protocol/client";
import {
  roleCan,
  type Dispatch,
  type Goal,
  type Principal,
  type Role,
  type Schedule,
  type ScheduleRun,
  type SessionId,
  type SessionSummary,
  type SubmitMode,
} from "../protocol/types";
import { sessionDisplayTitle } from "./Sidebar";

interface AutomationConsoleProps {
  open: boolean;
  client: OpenMirasimClient;
  principal: Principal | null;
  sessions: SessionSummary[];
  activeSessionId: SessionId | null;
  onClose: () => void;
  onRefreshSessions?: () => Promise<void>;
}

type AutoTab = "schedules" | "goals" | "dispatches";

function formatTs(ts: number): string {
  try {
    return new Date(ts).toLocaleString();
  } catch {
    return String(ts);
  }
}

const WEEKDAY_LABELS = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];

function describeSpec(s: Schedule): string {
  switch (s.spec.kind) {
    case "every":
      return `每 ${s.spec.interval_secs} 秒`;
    case "once":
      return `单次 @ ${formatTs(s.spec.at)}`;
    case "daily": {
      const hh = String(Math.floor(s.spec.minute_of_day / 60)).padStart(2, "0");
      const mm = String(s.spec.minute_of_day % 60).padStart(2, "0");
      const offsetMins = s.spec.tz_offset_mins ?? 0;
      const sign = offsetMins >= 0 ? "+" : "-";
      const absMins = Math.abs(offsetMins);
      const tzH = String(Math.floor(absMins / 60)).padStart(2, "0");
      const tzM = String(absMins % 60).padStart(2, "0");
      const days =
        s.spec.weekdays && s.spec.weekdays.length > 0 && s.spec.weekdays.length < 7
          ? ` (${s.spec.weekdays.map((d) => WEEKDAY_LABELS[d] ?? String(d)).join("/")})`
          : "";
      return `每日 ${hh}:${mm} UTC${sign}${tzH}:${tzM}${days}`;
    }
  }
}

function describeRunOutcome(run: ScheduleRun): string {
  switch (run.outcome.outcome) {
    case "delivered":
    case "session_started":
      return `已投递 -> ${run.outcome.session_id.slice(0, 12)} (${run.outcome.submit.outcome})`;
    case "skipped":
      return `已跳过 (${run.outcome.reason})`;
    case "failed":
      return `失败: ${run.outcome.error}`;
  }
}

export function AutomationConsole({
  open,
  client,
  principal,
  sessions,
  activeSessionId,
  onClose,
  onRefreshSessions,
}: AutomationConsoleProps) {
  const [tab, setTab] = useState<AutoTab>("schedules");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // Schedules
  const [schedules, setSchedules] = useState<Schedule[]>([]);
  const [selectedScheduleId, setSelectedScheduleId] = useState<string | null>(
    null
  );
  const [runs, setRuns] = useState<ScheduleRun[]>([]);
  const [schedName, setSchedName] = useState("");
  const [schedIntervalSecs, setSchedIntervalSecs] = useState("3600");
  const [schedTargetSession, setSchedTargetSession] = useState("");
  const [schedPrompt, setSchedPrompt] = useState("");

  // Goals
  const [goals, setGoals] = useState<Goal[]>([]);
  const [goalObjective, setGoalObjective] = useState("");
  const [goalMaxTurns, setGoalMaxTurns] = useState("5");

  // Dispatches
  const [dispatches, setDispatches] = useState<Dispatch[]>([]);
  const [dispatchTargetSession, setDispatchTargetSession] = useState("");
  const [dispatchMode, setDispatchMode] = useState<SubmitMode>("queue");
  const [dispatchPrompt, setDispatchPrompt] = useState("");

  const [submitting, setSubmitting] = useState(false);

  const role: Role = principal?.role ?? "viewer";
  const canPrompt = roleCan(role, "prompt_session");
  const canManageAllSchedules = roleCan(role, "manage_workspaces");
  const otherSessions = sessions.filter((s) => s.id !== activeSessionId);

  useEffect(() => {
    const valid = sessions.some((s) => s.id === schedTargetSession);
    if (!valid) {
      setSchedTargetSession(activeSessionId ?? sessions[0]?.id ?? "");
    }
  }, [activeSessionId, sessions, schedTargetSession]);

  useEffect(() => {
    const valid = otherSessions.some((s) => s.id === dispatchTargetSession);
    if (!valid) {
      setDispatchTargetSession(otherSessions[0]?.id ?? "");
    }
  }, [otherSessions, dispatchTargetSession]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  const loadRuns = useCallback(
    async (scheduleId: string) => {
      try {
        const list = await client.listScheduleRuns(scheduleId, 20);
        setRuns(list);
      } catch (err) {
        setError(err instanceof Error ? err.message : "加载运行记录失败");
      }
    },
    [client]
  );

  const loadAll = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const schedList = await client.listSchedules();
      setSchedules(schedList);
      if (selectedScheduleId) {
        const found = schedList.some((s) => s.id === selectedScheduleId);
        if (found) {
          await loadRuns(selectedScheduleId);
        } else {
          setSelectedScheduleId(null);
          setRuns([]);
        }
      }
      if (activeSessionId) {
        const [goalList, dispList] = await Promise.all([
          client.listGoals(activeSessionId),
          client.listDispatches(activeSessionId, 50),
        ]);
        setGoals(goalList);
        setDispatches(dispList);
      } else {
        setGoals([]);
        setDispatches([]);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "加载编排数据失败");
    } finally {
      setLoading(false);
    }
  }, [client, activeSessionId, selectedScheduleId, loadRuns]);

  useEffect(() => {
    if (open) {
      setNotice(null);
    }
  }, [open]);

  useEffect(() => {
    if (open) {
      void loadAll();
    }
  }, [open, loadAll]);

  if (!open) return null;

  const handleCreateSchedule = async (e: React.FormEvent) => {
    e.preventDefault();
    if (submitting || !schedName.trim() || !schedTargetSession || !schedPrompt.trim()) {
      return;
    }
    setError(null);
    setNotice(null);
    const parsedInterval = Number.parseInt(schedIntervalSecs.trim(), 10);
    if (!Number.isFinite(parsedInterval) || parsedInterval < 60 || parsedInterval > 31_536_000) {
      setError("触发间隔必须是 60 到 31536000 之间的整数秒");
      return;
    }
    setSubmitting(true);
    try {
      const created = await client.createSchedule({
        name: schedName.trim(),
        spec: {
          kind: "every",
          interval_secs: parsedInterval,
          start_at: Date.now() + parsedInterval * 1000,
        },
        target: {
          kind: "session",
          session_id: schedTargetSession,
          prompt: { content: [{ type: "text", text: schedPrompt.trim() }] },
          mode: "queue",
        },
        enabled: true,
        missed: "run_once",
        overlap: "allow",
      });
      setSchedName("");
      setSchedPrompt("");
      setSelectedScheduleId(created.id);
      setNotice(`已创建定时任务「${created.name}」`);
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "创建定时任务失败");
    } finally {
      setSubmitting(false);
    }
  };

  const handleToggleSchedule = async (s: Schedule) => {
    setError(null);
    try {
      await client.setScheduleEnabled(s.id, !s.enabled);
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "更新定时任务状态失败");
    }
  };

  const handleTriggerSchedule = async (s: Schedule) => {
    setError(null);
    setNotice(null);
    try {
      const run = await client.triggerSchedule(s.id);
      setSelectedScheduleId(s.id);
      setNotice(`已手动触发「${s.name}」：${describeRunOutcome(run)}`);
      await loadRuns(s.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : "手动触发失败");
    }
  };

  const handleDeleteSchedule = async (s: Schedule) => {
    setError(null);
    try {
      await client.deleteSchedule(s.id);
      if (selectedScheduleId === s.id) {
        setSelectedScheduleId(null);
        setRuns([]);
      }
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "删除定时任务失败");
    }
  };

  const handleSetGoal = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!activeSessionId || !goalObjective.trim()) return;
    setError(null);
    setNotice(null);
    const maxTurns = Math.min(
      50,
      Math.max(1, Number.parseInt(goalMaxTurns, 10) || 5)
    );
    try {
      const g = await client.setGoal(activeSessionId, {
        objective: goalObjective.trim(),
        max_turns: maxTurns,
      });
      setGoalObjective("");
      setNotice(`目标循环已启动（最多 ${g.max_turns} 轮，完成标记 ${g.done_marker}）`);
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "启动目标循环失败");
    }
  };

  const handleStopGoal = async (goalId: string) => {
    setError(null);
    try {
      await client.stopGoal(goalId);
      setNotice("目标循环已停止");
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "停止目标失败");
    }
  };

  const handleSendDispatch = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!activeSessionId || !dispatchTargetSession || !dispatchPrompt.trim()) {
      return;
    }
    setError(null);
    setNotice(null);
    try {
      const d = await client.createDispatch({
        from_session: activeSessionId,
        target: {
          kind: "session",
          session_id: dispatchTargetSession,
          mode: dispatchMode,
        },
        prompt: { content: [{ type: "text", text: dispatchPrompt.trim() }] },
      });
      setDispatchPrompt("");
      setNotice(`已派发指令至会话 ${d.to_session.slice(0, 12)}（depth=${d.depth}）`);
      await loadAll();
      if (onRefreshSessions) {
        await onRefreshSessions();
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "跨会话派发失败");
    }
  };

  const activeSession = sessions.find((s) => s.id === activeSessionId) ?? null;

  return (
    <div
      className="modal-backdrop"
      role="dialog"
      aria-modal="true"
      aria-labelledby="automation-console-title"
      id="automation-console-modal"
      onClick={onClose}
    >
      <div
        className="modal-card team-console-card"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="modal-head">
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <CalendarClock size={17} />
            <h2 id="automation-console-title">
              自动化编排（Schedules / Goals / Dispatch）
            </h2>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <button
              type="button"
              className="icon-btn"
              onClick={() => void loadAll()}
              disabled={loading}
              aria-label="刷新编排状态"
            >
              <RefreshCw size={14} className={loading ? "spin" : undefined} />
            </button>
            <button
              type="button"
              className="icon-btn"
              onClick={onClose}
              aria-label="关闭自动化编排面板"
            >
              <X size={16} />
            </button>
          </div>
        </div>

        <div className="drawer-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            id="tab-auto-schedules"
            aria-selected={tab === "schedules"}
            className={`drawer-tab ${tab === "schedules" ? "active" : ""}`}
            onClick={() => setTab("schedules")}
          >
            <CalendarClock size={13} />
            <span>定时任务 ({schedules.length})</span>
          </button>
          <button
            type="button"
            role="tab"
            id="tab-auto-goals"
            aria-selected={tab === "goals"}
            className={`drawer-tab ${tab === "goals" ? "active" : ""}`}
            onClick={() => setTab("goals")}
          >
            <Target size={13} />
            <span>目标循环 ({goals.length})</span>
          </button>
          <button
            type="button"
            role="tab"
            id="tab-auto-dispatches"
            aria-selected={tab === "dispatches"}
            className={`drawer-tab ${tab === "dispatches" ? "active" : ""}`}
            onClick={() => setTab("dispatches")}
          >
            <Send size={13} />
            <span>跨会话派发 ({dispatches.length})</span>
          </button>
        </div>

        {error && (
          <div className="error-banner" role="alert" style={{ marginTop: 10 }}>
            {error}
          </div>
        )}
        {notice && (
          <div className="hub-notice" role="status" style={{ marginTop: 10 }}>
            {notice}
          </div>
        )}

        <div className="team-console-body">
          {tab === "schedules" && (
            <div className="team-section">
              {canPrompt && (
                <form
                  className="hub-form-stack"
                  onSubmit={(e) => void handleCreateSchedule(e)}
                >
                  <div className="hub-form-row">
                    <input
                      id="sched-name-input"
                      className="input"
                      placeholder="定时任务名称（如 每日巡检）"
                      value={schedName}
                      onChange={(e) => setSchedName(e.target.value)}
                      required
                    />
                    <input
                      id="sched-interval-input"
                      className="input input-mono"
                      type="number"
                      min={60}
                      max={31536000}
                      value={schedIntervalSecs}
                      onChange={(e) => setSchedIntervalSecs(e.target.value)}
                      placeholder="间隔秒数 (>=60)"
                      style={{ maxWidth: 140 }}
                    />
                    <select
                      id="sched-target-session"
                      className="input"
                      value={schedTargetSession}
                      onChange={(e) => setSchedTargetSession(e.target.value)}
                    >
                      {sessions.map((s) => (
                        <option key={s.id} value={s.id}>
                          目标会话: {sessionDisplayTitle(s)}
                        </option>
                      ))}
                    </select>
                  </div>
                  <div className="hub-form-row">
                    <input
                      id="sched-prompt-input"
                      className="input"
                      placeholder="触发时向目标会话发送的 Prompt 指令…"
                      value={schedPrompt}
                      onChange={(e) => setSchedPrompt(e.target.value)}
                      required
                    />
                    <button
                      type="submit"
                      id="confirm-create-schedule-btn"
                      className="btn btn-primary btn-sm"
                      disabled={submitting || sessions.length === 0}
                    >
                      <CalendarClock size={13} />
                      <span>创建定时任务</span>
                    </button>
                  </div>
                </form>
              )}

              <div className="team-list" id="schedules-list">
                {schedules.length === 0 ? (
                  <div className="muted-empty">暂无定时任务</div>
                ) : (
                  schedules.map((s) => (
                    <div
                      key={s.id}
                      className="team-row"
                      data-schedule-id={s.id}
                    >
                      <div className="team-row-main">
                        <div className="team-row-title">
                          <span>{s.name}</span>
                          <span className={`chip ${s.enabled ? "ok" : ""}`}>
                            {s.enabled ? "运行中" : "已暂停"}
                          </span>
                          <span className="chip">{describeSpec(s)}</span>
                        </div>
                        <div className="team-row-sub">
                          <span>
                            {s.target.kind === "session"
                              ? `会话 ${s.target.session_id.slice(0, 12)}`
                              : `新建 ${s.target.config.harness_id} 会话`}
                          </span>
                          {s.next_run_at && (
                            <span>下次触发: {formatTs(s.next_run_at)}</span>
                          )}
                        </div>
                      </div>
                      <div className="team-row-actions">
                        {(canManageAllSchedules ||
                          (canPrompt &&
                            (!s.created_by || s.created_by === principal?.member_id))) && (
                          <>
                            <button
                              type="button"
                              className="btn btn-sm"
                              data-trigger-schedule={s.id}
                              onClick={() => void handleTriggerSchedule(s)}
                              title="立即手动触发一次"
                            >
                              <Zap size={12} />
                              <span>立即触发</span>
                            </button>
                            <button
                              type="button"
                              className="btn btn-sm"
                              onClick={() => void handleToggleSchedule(s)}
                              title={s.enabled ? "暂停" : "恢复"}
                            >
                              {s.enabled ? <Pause size={12} /> : <Play size={12} />}
                            </button>
                            <button
                              type="button"
                              className="btn btn-sm"
                              onClick={() => void handleDeleteSchedule(s)}
                              aria-label={`删除定时任务 ${s.name}`}
                            >
                              <Trash2 size={12} />
                            </button>
                          </>
                        )}
                        <button
                          type="button"
                          className="btn btn-sm"
                          data-view-runs={s.id}
                          onClick={() => {
                            setSelectedScheduleId(s.id);
                            void loadRuns(s.id);
                          }}
                        >
                          运行记录
                        </button>
                      </div>
                    </div>
                  ))
                )}
              </div>

              {selectedScheduleId && (
                <div className="hub-subpanel" id="schedule-runs-list">
                  <div className="hub-subpanel-title">
                    最近运行记录（{runs.length} 条）
                  </div>
                  {runs.length === 0 ? (
                    <div className="muted-empty">尚无运行记录</div>
                  ) : (
                    runs.map((r) => (
                      <div key={r.id} className="hub-run-item">
                        <span
                          className={`chip ${
                            r.outcome.outcome === "delivered" ||
                            r.outcome.outcome === "session_started"
                              ? "ok"
                              : "warn"
                          }`}
                        >
                          {r.manual ? "手动" : "定时"} · {r.outcome.outcome}
                        </span>
                        <span>{describeRunOutcome(r)}</span>
                        <span className="muted-ts">{formatTs(r.fired_at)}</span>
                      </div>
                    ))
                  )}
                </div>
              )}
            </div>
          )}

          {tab === "goals" && (
            <div className="team-section">
              {!activeSession ? (
                <div className="muted-empty">请先在左侧选择一个会话</div>
              ) : (
                <>
                  <div className="hub-context-bar">
                    当前会话：<strong>{sessionDisplayTitle(activeSession)}</strong>
                    （Agent 输出 <code>[[goal:done]]</code> 即自动收敛完成）
                  </div>

                  {canPrompt && (
                    <form
                      className="hub-form-stack"
                      onSubmit={(e) => void handleSetGoal(e)}
                    >
                      <div className="hub-form-row">
                        <input
                          id="goal-objective-input"
                          className="input"
                          placeholder="输入目标描述（如：修复所有单元测试并确认 clippy 零警告）"
                          value={goalObjective}
                          onChange={(e) => setGoalObjective(e.target.value)}
                          required
                        />
                        <input
                          id="goal-max-turns-input"
                          className="input input-mono"
                          type="number"
                          min={1}
                          max={50}
                          value={goalMaxTurns}
                          onChange={(e) => setGoalMaxTurns(e.target.value)}
                          style={{ maxWidth: 100 }}
                          title="最大迭代轮数 (1-50)"
                        />
                        <button
                          type="submit"
                          id="confirm-set-goal-btn"
                          className="btn btn-primary btn-sm"
                        >
                          <Target size={13} />
                          <span>启动目标循环</span>
                        </button>
                      </div>
                    </form>
                  )}

                  <div className="team-list" id="goals-list">
                    {goals.length === 0 ? (
                      <div className="muted-empty">该会话暂无目标循环记录</div>
                    ) : (
                      goals.map((g) => (
                        <div key={g.id} className="team-row" data-goal-row={g.id}>
                          <div className="team-row-main">
                            <div className="team-row-title">
                              <span>{g.objective}</span>
                              <span
                                className={`chip ${
                                  g.status === "achieved"
                                    ? "ok"
                                    : g.status === "active"
                                      ? "warn"
                                      : ""
                                }`}
                              >
                                {g.status}
                              </span>
                            </div>
                            <div className="team-row-sub">
                              <span>
                                轮次: {g.turns_used} / {g.max_turns}
                              </span>
                              {g.ended_reason && (
                                <span>结束原因: {g.ended_reason}</span>
                              )}
                              <span>{formatTs(g.created_at)}</span>
                            </div>
                          </div>
                          {canPrompt && g.status === "active" && (
                            <button
                              type="button"
                              className="btn btn-sm"
                              data-stop-goal={g.id}
                              onClick={() => void handleStopGoal(g.id)}
                            >
                              <Square size={11} />
                              <span>停止</span>
                            </button>
                          )}
                        </div>
                      ))
                    )}
                  </div>
                </>
              )}
            </div>
          )}

          {tab === "dispatches" && (
            <div className="team-section">
              {!activeSession ? (
                <div className="muted-empty">请先在左侧选择一个源会话</div>
              ) : (
                <>
                  <div className="hub-context-bar">
                    源会话：<strong>{sessionDisplayTitle(activeSession)}</strong>
                    （权限按当前成员身份校验，链深度上限 3 跳）
                  </div>

                  {canPrompt && (
                    <form
                      className="hub-form-stack"
                      onSubmit={(e) => void handleSendDispatch(e)}
                    >
                      <div className="hub-form-row">
                        <select
                          id="dispatch-target-session"
                          className="input"
                          value={dispatchTargetSession}
                          onChange={(e) => setDispatchTargetSession(e.target.value)}
                        >
                          {otherSessions.length === 0 ? (
                            <option value="">（无其他目标会话）</option>
                          ) : (
                            otherSessions.map((s) => (
                              <option key={s.id} value={s.id}>
                                派发至: {sessionDisplayTitle(s)}
                              </option>
                            ))
                          )}
                        </select>
                        <select
                          id="dispatch-mode-select"
                          className="input"
                          value={dispatchMode}
                          onChange={(e) =>
                            setDispatchMode(e.target.value as SubmitMode)
                          }
                          style={{ maxWidth: 140 }}
                        >
                          <option value="queue">排队 (Queue)</option>
                          <option value="next">插队 (Next)</option>
                          <option value="steer">转向 (Steer)</option>
                        </select>
                      </div>
                      <div className="hub-form-row">
                        <input
                          id="dispatch-prompt-input"
                          className="input"
                          placeholder="要派发给目标会话的协同任务指令…"
                          value={dispatchPrompt}
                          onChange={(e) => setDispatchPrompt(e.target.value)}
                          required
                        />
                        <button
                          type="submit"
                          id="confirm-send-dispatch-btn"
                          className="btn btn-primary btn-sm"
                          disabled={otherSessions.length === 0}
                        >
                          <Send size={13} />
                          <span>立即派发</span>
                        </button>
                      </div>
                    </form>
                  )}

                  <div className="team-list" id="dispatches-list">
                    {dispatches.length === 0 ? (
                      <div className="muted-empty">该会话暂无跨会话派发记录</div>
                    ) : (
                      dispatches.map((d) => (
                        <div key={d.id} className="team-row" data-dispatch-row={d.id}>
                          <div className="team-row-main">
                            <div className="team-row-title">
                              <span className="input-mono">
                                {d.from_session.slice(0, 10)} →{" "}
                                {d.to_session.slice(0, 10)}
                              </span>
                              <span className="chip ok">depth {d.depth}</span>
                              <span className="chip">{d.submit.outcome}</span>
                            </div>
                            <div className="team-row-sub">
                              <span>执行身份: {d.member_id.slice(0, 12)}</span>
                              <span>{formatTs(d.created_at)}</span>
                            </div>
                          </div>
                        </div>
                      ))
                    )}
                  </div>
                </>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
