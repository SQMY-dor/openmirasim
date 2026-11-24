import {
  Activity,
  BookOpen,
  Check,
  Copy,
  Layers,
  Plug,
  Plus,
  RefreshCw,
  Trash2,
  X,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { OpenMirasimClient } from "../protocol/client";
import {
  roleCan,
  type EffectiveHubResponse,
  type HubScopeLevel,
  type McpProbeResult,
  type McpServerRecord,
  type McpTransport,
  type Principal,
  type Role,
  type SkillLifecycle,
  type SkillRecord,
  type WorkspaceSummary,
} from "../protocol/types";

interface HubConsoleProps {
  open: boolean;
  client: OpenMirasimClient;
  principal: Principal | null;
  workspaces: WorkspaceSummary[];
  onClose: () => void;
}

type HubTab = "mcp" | "skills" | "effective";

const LIFECYCLES: SkillLifecycle[] = ["draft", "active", "deprecated", "archived"];

/** Splits a shell-like argument string while preserving single- or double-quoted tokens. */
function parseShellArgs(input: string): string[] {
  const out: string[] = [];
  let current = "";
  let quote: "'" | '"' | null = null;
  let escape = false;
  for (let i = 0; i < input.length; i += 1) {
    const ch = input[i];
    if (escape) {
      current += ch;
      escape = false;
      continue;
    }
    if (ch === "\\" && quote !== "'") {
      escape = true;
      continue;
    }
    if (quote) {
      if (ch === quote) {
        quote = null;
      } else {
        current += ch;
      }
      continue;
    }
    if (ch === "'" || ch === '"') {
      quote = ch;
    } else if (/\s/.test(ch)) {
      if (current.length > 0) {
        out.push(current);
        current = "";
      }
    } else {
      current += ch;
    }
  }
  if (current.length > 0) {
    out.push(current);
  }
  return out;
}

export function HubConsole({
  open,
  client,
  principal,
  workspaces,
  onClose,
}: HubConsoleProps) {
  const [tab, setTab] = useState<HubTab>("mcp");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // MCP Hub state
  const [servers, setServers] = useState<McpServerRecord[]>([]);
  const [probes, setProbes] = useState<Record<string, McpProbeResult>>({});
  const [mcpName, setMcpName] = useState("");
  const [mcpScope, setMcpScope] = useState<HubScopeLevel>("space");
  const [mcpWorkspaceId, setMcpWorkspaceId] = useState("");
  const [mcpTransport, setMcpTransport] = useState<McpTransport>("stdio");
  const [mcpCommand, setMcpCommand] = useState("");
  const [mcpArgs, setMcpArgs] = useState("");
  const [mcpUrl, setMcpUrl] = useState("");
  const [mcpEnvKeys, setMcpEnvKeys] = useState("");
  const [mcpDesc, setMcpDesc] = useState("");
  const [mcpGlobalAvailable, setMcpGlobalAvailable] = useState(true);

  // Skills state
  const [skills, setSkills] = useState<SkillRecord[]>([]);
  const [skillName, setSkillName] = useState("");
  const [skillScope, setSkillScope] = useState<HubScopeLevel>("space");
  const [skillWorkspaceId, setSkillWorkspaceId] = useState("");
  const [skillVersion, setSkillVersion] = useState("1.0.0");
  const [skillLifecycle, setSkillLifecycle] = useState<SkillLifecycle>("draft");
  const [skillDesc, setSkillDesc] = useState("");
  const [skillTags, setSkillTags] = useState("");
  const [skillInstructions, setSkillInstructions] = useState("");

  // Effective two-tier resolution view
  const [effectiveWorkspaceId, setEffectiveWorkspaceId] = useState("");
  const effectiveInitRef = useRef(false);
  const [effective, setEffective] = useState<EffectiveHubResponse | null>(null);
  const [copiedJson, setCopiedJson] = useState(false);

  const role: Role = principal?.role ?? "viewer";
  const canManageSpaceHub = roleCan(role, "manage_workspaces");
  const canManageWorkspaceHub = roleCan(role, "create_session");

  useEffect(() => {
    if (!canManageSpaceHub && canManageWorkspaceHub) {
      setMcpScope("workspace");
      setSkillScope("workspace");
    }
  }, [canManageSpaceHub, canManageWorkspaceHub]);

  useEffect(() => {
    if (workspaces.length > 0) {
      if (!mcpWorkspaceId || !workspaces.some((w) => w.id === mcpWorkspaceId)) {
        setMcpWorkspaceId(workspaces[0].id);
      }
      if (!skillWorkspaceId || !workspaces.some((w) => w.id === skillWorkspaceId)) {
        setSkillWorkspaceId(workspaces[0].id);
      }
      if (!effectiveInitRef.current) {
        effectiveInitRef.current = true;
        setEffectiveWorkspaceId(workspaces[0].id);
      } else if (
        effectiveWorkspaceId &&
        !workspaces.some((w) => w.id === effectiveWorkspaceId)
      ) {
        setEffectiveWorkspaceId("");
      }
    }
  }, [workspaces, mcpWorkspaceId, skillWorkspaceId, effectiveWorkspaceId]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  const loadEffective = useCallback(
    async (wsId: string) => {
      try {
        const res = await client.getEffectiveHub(wsId || undefined);
        setEffective(res);
      } catch (err) {
        setError(err instanceof Error ? err.message : "加载生效视图失败");
      }
    },
    [client]
  );

  const loadAll = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [mcpList, skillList] = await Promise.all([
        client.listMcpServers(),
        client.listSkills(),
      ]);
      setServers(mcpList);
      setSkills(skillList);
      await loadEffective(effectiveWorkspaceId);
    } catch (err) {
      setError(err instanceof Error ? err.message : "加载 MCP Hub / Skills 失败");
    } finally {
      setLoading(false);
    }
  }, [client, effectiveWorkspaceId, loadEffective]);

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

  const canWriteScope = (scope: HubScopeLevel) =>
    scope === "space" ? canManageSpaceHub : canManageWorkspaceHub;

  const canWriteMcp = (s: McpServerRecord) => {
    if (s.scope === "space") return canManageSpaceHub;
    if (canManageSpaceHub) return true;
    return (
      canManageWorkspaceHub &&
      (!s.owner_member_id || s.owner_member_id === principal?.member_id)
    );
  };

  const handleCreateMcp = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!mcpName.trim()) return;
    setError(null);
    setNotice(null);
    const envKeys = mcpEnvKeys
      .split(",")
      .map((k) => k.trim())
      .filter(Boolean);
    const args = parseShellArgs(mcpArgs.trim());
    try {
      const rec = await client.createMcpServer({
        name: mcpName.trim(),
        scope: mcpScope,
        workspace_id: mcpScope === "workspace" ? mcpWorkspaceId : undefined,
        transport: mcpTransport,
        command: mcpTransport === "stdio" ? mcpCommand.trim() : undefined,
        args: mcpTransport === "stdio" ? args : [],
        url: mcpTransport !== "stdio" ? mcpUrl.trim() : undefined,
        env_keys: envKeys,
        description: mcpDesc.trim(),
        enabled: true,
        ...(mcpScope === "space"
          ? { global_available: mcpGlobalAvailable }
          : {}),
      });
      setMcpName("");
      setMcpCommand("");
      setMcpArgs("");
      setMcpUrl("");
      setMcpEnvKeys("");
      setMcpDesc("");
      setNotice(`已注册 ${rec.scope.toUpperCase()} 级 MCP 服务「${rec.name}」`);
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "注册 MCP 服务失败");
    }
  };

  const handleToggleGlobalMcp = async (s: McpServerRecord) => {
    if (s.scope !== "space" || !canManageSpaceHub) return;
    setError(null);
    try {
      const nextGlobal = s.global_available === false;
      await client.updateMcpServer(s.id, { global_available: nextGlobal });
      setNotice(
        `已将 Space MCP「${s.name}」设为${nextGlobal ? "全局可用" : "仅管理员可见"}`
      );
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "更新 MCP 全局可用性失败");
    }
  };

  const handleProbeMcp = async (id: string) => {
    setError(null);
    try {
      const res = await client.probeMcpServer(id);
      setProbes((prev) => ({ ...prev, [id]: res }));
    } catch (err) {
      setError(err instanceof Error ? err.message : "探测 MCP 服务失败");
    }
  };

  const handleToggleMcp = async (s: McpServerRecord) => {
    setError(null);
    try {
      await client.updateMcpServer(s.id, { enabled: !s.enabled });
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "切换 MCP 状态失败");
    }
  };

  const handleDeleteMcp = async (s: McpServerRecord) => {
    setError(null);
    try {
      await client.deleteMcpServer(s.id);
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "删除 MCP 服务失败");
    }
  };

  const handleCreateSkill = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!skillName.trim() || !skillInstructions.trim()) return;
    setError(null);
    setNotice(null);
    const tags = skillTags
      .split(",")
      .map((t) => t.trim())
      .filter(Boolean);
    try {
      const rec = await client.createSkill({
        name: skillName.trim(),
        scope: skillScope,
        workspace_id: skillScope === "workspace" ? skillWorkspaceId : undefined,
        version: skillVersion.trim() || "1.0.0",
        description: skillDesc.trim(),
        tags,
        instructions: skillInstructions.trim(),
        lifecycle: skillLifecycle,
        enabled: skillLifecycle !== "archived",
      });
      setSkillName("");
      setSkillDesc("");
      setSkillTags("");
      setSkillInstructions("");
      setNotice(`已安装 Skill「${rec.name}」v${rec.version} (${rec.lifecycle})`);
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "安装 Skill 失败");
    }
  };

  const handleChangeSkillLifecycle = async (
    sk: SkillRecord,
    lifecycle: SkillLifecycle
  ) => {
    setError(null);
    try {
      await client.updateSkill(sk.id, { lifecycle });
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "更新 Skill 生命周期失败");
    }
  };

  const handleToggleSkill = async (sk: SkillRecord) => {
    setError(null);
    try {
      await client.updateSkill(sk.id, { enabled: !sk.enabled });
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "切换 Skill 启用状态失败");
    }
  };

  const handleDeleteSkill = async (sk: SkillRecord) => {
    setError(null);
    try {
      await client.deleteSkill(sk.id);
      await loadAll();
    } catch (err) {
      setError(err instanceof Error ? err.message : "删除 Skill 失败");
    }
  };

  const wsName = (id?: string) =>
    workspaces.find((w) => w.id === id)?.name ?? id?.slice(0, 10) ?? "Workspace";

  const copyEffectiveJson = async () => {
    if (!effective) return;
    try {
      await navigator.clipboard.writeText(
        JSON.stringify(effective.mcp_client_config, null, 2)
      );
      setCopiedJson(true);
      window.setTimeout(() => setCopiedJson(false), 1800);
    } catch {
      // ignore clipboard restrictions
    }
  };

  return (
    <div
      className="modal-backdrop"
      role="dialog"
      aria-modal="true"
      aria-labelledby="hub-console-title"
      id="hub-console-modal"
      onClick={onClose}
    >
      <div
        className="modal-card team-console-card"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="modal-head">
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <Plug size={17} />
            <h2 id="hub-console-title">
              两级 MCP Hub 与 Skills 生命周期管理
            </h2>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <button
              type="button"
              className="icon-btn"
              onClick={() => void loadAll()}
              disabled={loading}
              aria-label="刷新 Hub 列表"
            >
              <RefreshCw size={14} className={loading ? "spin" : undefined} />
            </button>
            <button
              type="button"
              className="icon-btn"
              onClick={onClose}
              aria-label="关闭 MCP Hub 面板"
            >
              <X size={16} />
            </button>
          </div>
        </div>

        <div className="drawer-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            id="tab-hub-mcp"
            aria-selected={tab === "mcp"}
            className={`drawer-tab ${tab === "mcp" ? "active" : ""}`}
            onClick={() => setTab("mcp")}
          >
            <Plug size={13} />
            <span>MCP Hub ({servers.length})</span>
          </button>
          <button
            type="button"
            role="tab"
            id="tab-hub-skills"
            aria-selected={tab === "skills"}
            className={`drawer-tab ${tab === "skills" ? "active" : ""}`}
            onClick={() => setTab("skills")}
          >
            <BookOpen size={13} />
            <span>Skills ({skills.length})</span>
          </button>
          <button
            type="button"
            role="tab"
            id="tab-hub-effective"
            aria-selected={tab === "effective"}
            className={`drawer-tab ${tab === "effective" ? "active" : ""}`}
            onClick={() => {
              setTab("effective");
              void loadEffective(effectiveWorkspaceId);
            }}
          >
            <Layers size={13} />
            <span>两级生效视图</span>
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
          {tab === "mcp" && (
            <div className="team-section">
              {canManageWorkspaceHub && (
                <form
                  className="hub-form-stack"
                  onSubmit={(e) => void handleCreateMcp(e)}
                >
                  <div className="hub-form-row">
                    <input
                      id="mcp-name-input"
                      className="input input-mono"
                      placeholder="MCP 名称（如 github / postgres）"
                      value={mcpName}
                      onChange={(e) => setMcpName(e.target.value)}
                      required
                    />
                    <select
                      id="mcp-scope-select"
                      className="input"
                      value={mcpScope}
                      onChange={(e) =>
                        setMcpScope(e.target.value as HubScopeLevel)
                      }
                      style={{ maxWidth: 165 }}
                    >
                      {canManageSpaceHub && (
                        <option value="space">Space 级（全局默认）</option>
                      )}
                      <option value="workspace">Workspace 级（仓库覆盖）</option>
                    </select>
                    {mcpScope === "workspace" && (
                      <select
                        id="mcp-workspace-select"
                        className="input"
                        value={mcpWorkspaceId}
                        onChange={(e) => setMcpWorkspaceId(e.target.value)}
                        required
                      >
                        {workspaces.length === 0 ? (
                          <option value="">（请先注册 Git 仓库）</option>
                        ) : (
                          workspaces.map((w) => (
                            <option key={w.id} value={w.id}>
                              {w.name}
                            </option>
                          ))
                        )}
                      </select>
                    )}
                    <select
                      id="mcp-transport-select"
                      className="input"
                      value={mcpTransport}
                      onChange={(e) =>
                        setMcpTransport(e.target.value as McpTransport)
                      }
                      style={{ maxWidth: 155 }}
                    >
                      <option value="stdio">stdio</option>
                      <option value="streamable_http">streamable_http</option>
                      <option value="sse">sse</option>
                    </select>
                  </div>

                  <div className="hub-form-row">
                    {mcpTransport === "stdio" ? (
                      <>
                        <input
                          id="mcp-command-input"
                          className="input input-mono"
                          placeholder="可执行命令（如 npx 或 /usr/local/bin/mcp-server）"
                          value={mcpCommand}
                          onChange={(e) => setMcpCommand(e.target.value)}
                          required
                        />
                        <input
                          id="mcp-args-input"
                          className="input input-mono"
                          placeholder="参数（空格分隔）"
                          value={mcpArgs}
                          onChange={(e) => setMcpArgs(e.target.value)}
                        />
                      </>
                    ) : (
                      <input
                        id="mcp-url-input"
                        className="input input-mono"
                        type="url"
                        placeholder="https://mcp.internal.example/endpoint"
                        value={mcpUrl}
                        onChange={(e) => setMcpUrl(e.target.value)}
                        required
                      />
                    )}
                  </div>

                  <div className="hub-form-row">
                    <input
                      id="mcp-env-keys-input"
                      className="input input-mono"
                      placeholder="环境变量名（逗号分隔，仅填键名如 GITHUB_TOKEN，禁填明文密钥）"
                      value={mcpEnvKeys}
                      onChange={(e) => setMcpEnvKeys(e.target.value)}
                    />
                    <input
                      id="mcp-desc-input"
                      className="input"
                      placeholder="简要说明"
                      value={mcpDesc}
                      onChange={(e) => setMcpDesc(e.target.value)}
                      style={{ maxWidth: 200 }}
                    />
                    <button
                      type="submit"
                      id="confirm-create-mcp-btn"
                      className="btn btn-primary btn-sm"
                      disabled={
                        mcpScope === "workspace" && workspaces.length === 0
                      }
                    >
                      <Plus size={13} />
                      <span>注册 MCP</span>
                    </button>
                  </div>

                  {mcpScope === "space" && canManageSpaceHub && (
                    <label
                      style={{
                        display: "inline-flex",
                        alignItems: "center",
                        gap: 6,
                        fontSize: 12,
                        color: "var(--text-muted)",
                      }}
                    >
                      <input
                        id="mcp-global-available-toggle"
                        type="checkbox"
                        checked={mcpGlobalAvailable}
                        onChange={(e) => setMcpGlobalAvailable(e.target.checked)}
                      />
                      <span>全局可用（允许 Space 内所有成员在生效视图与会话中使用）</span>
                    </label>
                  )}
                </form>
              )}

              <div className="team-list" id="mcp-servers-list">
                {servers.length === 0 ? (
                  <div className="muted-empty">暂无已注册 MCP 服务</div>
                ) : (
                  servers.map((s) => {
                    const probe = probes[s.id];
                    const writable = canWriteMcp(s);
                    const isGlobal = s.scope === "space" && s.global_available !== false;
                    return (
                      <div key={s.id} className="team-row" data-mcp-id={s.id}>
                        <div className="team-row-main">
                          <div className="team-row-title">
                            <span className="input-mono">{s.name}</span>
                            <span className={`chip ${s.scope === "workspace" ? "warn" : "ok"}`}>
                              {s.scope === "space"
                                ? "SPACE"
                                : `WS:${wsName(s.workspace_id)}`}
                            </span>
                            {s.scope === "space" ? (
                              <span className={`chip ${isGlobal ? "ok" : "warn"}`}>
                                {isGlobal ? "全局可用" : "仅管理员"}
                              </span>
                            ) : (
                              <span className="chip">
                                成员私有{s.owner_member_id ? ` · ${s.owner_member_id.slice(0, 10)}` : ""}
                              </span>
                            )}
                            <span className="chip">{s.transport}</span>
                            <span className={`chip ${s.enabled ? "ok" : ""}`}>
                              {s.enabled ? "启用" : "停用"}
                            </span>
                            {probe && (
                              <span
                                className={`chip ${probe.reachable ? "ok" : "err"}`}
                                data-mcp-probe={s.id}
                              >
                                {probe.reachable ? "就绪" : "不可用"} · {probe.detail}
                              </span>
                            )}
                          </div>
                          <div className="team-row-sub">
                            <span className="input-mono">
                              {s.transport === "stdio"
                                ? `${s.command ?? ""} ${s.args.join(" ")}`.trim()
                                : s.url}
                            </span>
                            {s.env_keys.length > 0 && (
                              <span>env: {s.env_keys.join(", ")}</span>
                            )}
                            {s.description && <span>{s.description}</span>}
                          </div>
                        </div>
                        <div className="team-row-actions">
                          <button
                            type="button"
                            className="btn btn-sm"
                            data-probe-mcp={s.id}
                            onClick={() => void handleProbeMcp(s.id)}
                          >
                            <Activity size={12} />
                            <span>探测</span>
                          </button>
                          {s.scope === "space" && canManageSpaceHub && (
                            <button
                              type="button"
                              className="btn btn-sm"
                              data-toggle-global-mcp={s.id}
                              onClick={() => void handleToggleGlobalMcp(s)}
                              title="管理员控制该 Space MCP 是否对全员全局可用"
                            >
                              {isGlobal ? "收回全局" : "开放全局"}
                            </button>
                          )}
                          {writable && (
                            <>
                              <button
                                type="button"
                                className="btn btn-sm"
                                onClick={() => void handleToggleMcp(s)}
                              >
                                {s.enabled ? "停用" : "启用"}
                              </button>
                              <button
                                type="button"
                                className="btn btn-sm"
                                onClick={() => void handleDeleteMcp(s)}
                                aria-label={`删除 MCP ${s.name}`}
                              >
                                <Trash2 size={12} />
                              </button>
                            </>
                          )}
                        </div>
                      </div>
                    );
                  })
                )}
              </div>
            </div>
          )}

          {tab === "skills" && (
            <div className="team-section">
              {canManageWorkspaceHub && (
                <form
                  className="hub-form-stack"
                  onSubmit={(e) => void handleCreateSkill(e)}
                >
                  <div className="hub-form-row">
                    <input
                      id="skill-name-input"
                      className="input input-mono"
                      placeholder="Skill 标识（如 rust-reviewer）"
                      value={skillName}
                      onChange={(e) => setSkillName(e.target.value)}
                      required
                    />
                    <input
                      id="skill-version-input"
                      className="input input-mono"
                      placeholder="版本"
                      value={skillVersion}
                      onChange={(e) => setSkillVersion(e.target.value)}
                      style={{ maxWidth: 96 }}
                    />
                    <select
                      id="skill-scope-select"
                      className="input"
                      value={skillScope}
                      onChange={(e) =>
                        setSkillScope(e.target.value as HubScopeLevel)
                      }
                      style={{ maxWidth: 160 }}
                    >
                      {canManageSpaceHub && (
                        <option value="space">Space 级</option>
                      )}
                      <option value="workspace">Workspace 级</option>
                    </select>
                    {skillScope === "workspace" && (
                      <select
                        id="skill-workspace-select"
                        className="input"
                        value={skillWorkspaceId}
                        onChange={(e) => setSkillWorkspaceId(e.target.value)}
                        required
                      >
                        {workspaces.map((w) => (
                          <option key={w.id} value={w.id}>
                            {w.name}
                          </option>
                        ))}
                      </select>
                    )}
                    <select
                      id="skill-lifecycle-select"
                      className="input"
                      value={skillLifecycle}
                      onChange={(e) =>
                        setSkillLifecycle(e.target.value as SkillLifecycle)
                      }
                      style={{ maxWidth: 120 }}
                    >
                      {LIFECYCLES.map((lc) => (
                        <option key={lc} value={lc}>
                          {lc}
                        </option>
                      ))}
                    </select>
                  </div>

                  <div className="hub-form-row">
                    <input
                      id="skill-desc-input"
                      className="input"
                      placeholder="Skill 用途说明"
                      value={skillDesc}
                      onChange={(e) => setSkillDesc(e.target.value)}
                    />
                    <input
                      id="skill-tags-input"
                      className="input"
                      placeholder="标签（逗号分隔）"
                      value={skillTags}
                      onChange={(e) => setSkillTags(e.target.value)}
                      style={{ maxWidth: 180 }}
                    />
                  </div>

                  <div className="hub-form-row">
                    <textarea
                      id="skill-instructions-input"
                      className="input input-mono"
                      rows={3}
                      placeholder="# SKILL.md 指令内容…"
                      value={skillInstructions}
                      onChange={(e) => setSkillInstructions(e.target.value)}
                      required
                    />
                    <button
                      type="submit"
                      id="confirm-create-skill-btn"
                      className="btn btn-primary btn-sm"
                      disabled={
                        skillScope === "workspace" && workspaces.length === 0
                      }
                    >
                      <Plus size={13} />
                      <span>安装 Skill</span>
                    </button>
                  </div>
                </form>
              )}

              <div className="team-list" id="skills-list">
                {skills.length === 0 ? (
                  <div className="muted-empty">暂无已安装 Skills</div>
                ) : (
                  skills.map((sk) => {
                    const writable = canWriteScope(sk.scope);
                    return (
                      <div key={sk.id} className="team-row" data-skill-id={sk.id}>
                        <div className="team-row-main">
                          <div className="team-row-title">
                            <span className="input-mono">{sk.name}</span>
                            <span className="chip">v{sk.version}</span>
                            <span className={`chip ${sk.scope === "workspace" ? "warn" : "ok"}`}>
                              {sk.scope === "space"
                                ? "SPACE"
                                : `WS:${wsName(sk.workspace_id)}`}
                            </span>
                            <span
                              className={`chip ${
                                sk.lifecycle === "active"
                                  ? "ok"
                                  : sk.lifecycle === "deprecated"
                                    ? "warn"
                                    : ""
                              }`}
                            >
                              {sk.lifecycle}
                            </span>
                            <span className={`chip ${sk.enabled ? "ok" : ""}`}>
                              {sk.enabled ? "启用" : "停用"}
                            </span>
                          </div>
                          <div className="team-row-sub">
                            {sk.description && <span>{sk.description}</span>}
                            <span className="input-mono">
                              {sk.instructions.slice(0, 90)}
                              {sk.instructions.length > 90 ? "…" : ""}
                            </span>
                          </div>
                        </div>
                        <div className="team-row-actions">
                          {writable && (
                            <>
                              <select
                                className="input"
                                data-skill-lifecycle={sk.id}
                                value={sk.lifecycle}
                                onChange={(e) =>
                                  void handleChangeSkillLifecycle(
                                    sk,
                                    e.target.value as SkillLifecycle
                                  )
                                }
                                style={{ maxWidth: 115, padding: "4px 6px" }}
                              >
                                {LIFECYCLES.map((lc) => (
                                  <option key={lc} value={lc}>
                                    {lc}
                                  </option>
                                ))}
                              </select>
                              <button
                                type="button"
                                className="btn btn-sm"
                                disabled={sk.lifecycle === "archived"}
                                onClick={() => void handleToggleSkill(sk)}
                              >
                                {sk.enabled ? "停用" : "启用"}
                              </button>
                              <button
                                type="button"
                                className="btn btn-sm"
                                onClick={() => void handleDeleteSkill(sk)}
                                aria-label={`删除 Skill ${sk.name}`}
                              >
                                <Trash2 size={12} />
                              </button>
                            </>
                          )}
                        </div>
                      </div>
                    );
                  })
                )}
              </div>
            </div>
          )}

          {tab === "effective" && (
            <div className="team-section">
              <div className="hub-form-row">
                <select
                  id="effective-workspace-select"
                  className="input"
                  value={effectiveWorkspaceId}
                  onChange={(e) => {
                    setEffectiveWorkspaceId(e.target.value);
                    void loadEffective(e.target.value);
                  }}
                >
                  <option value="">仅 Space 全局默认</option>
                  {workspaces.map((w) => (
                    <option key={w.id} value={w.id}>
                      Workspace 合并视图: {w.name}
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  className="btn btn-sm"
                  onClick={() => void copyEffectiveJson()}
                >
                  {copiedJson ? <Check size={12} /> : <Copy size={12} />}
                  <span>{copiedJson ? "已复制 mcpServers" : "复制客户端 JSON"}</span>
                </button>
              </div>

              {effective && (
                <>
                  <div className="hub-subpanel" id="effective-mcp-list">
                    <div className="hub-subpanel-title">
                      最终生效 MCP 服务（{effective.mcp_servers.length}）
                    </div>
                    {effective.mcp_servers.length === 0 ? (
                      <div className="muted-empty">无生效 MCP 服务</div>
                    ) : (
                      effective.mcp_servers.map((m) => (
                        <div key={m.id} className="hub-run-item">
                          <span className="input-mono">{m.name}</span>
                          <span className="chip">{m.scope.toUpperCase()}</span>
                          <span className="chip">{m.transport}</span>
                          {m.overridden_space_default && (
                            <span className="chip warn">覆盖 Space 默认</span>
                          )}
                        </div>
                      ))
                    )}
                  </div>

                  <div className="hub-subpanel" id="effective-skills-list">
                    <div className="hub-subpanel-title">
                      最终生效 Skills（{effective.skills.length}）
                    </div>
                    {effective.skills.length === 0 ? (
                      <div className="muted-empty">无生效 Skills</div>
                    ) : (
                      effective.skills.map((sk) => (
                        <div key={sk.id} className="hub-run-item">
                          <span className="input-mono">
                            {sk.name} v{sk.version}
                          </span>
                          <span className="chip">{sk.lifecycle}</span>
                          <span className="chip">{sk.scope.toUpperCase()}</span>
                          {sk.overridden_space_default && (
                            <span className="chip warn">覆盖 Space 默认</span>
                          )}
                        </div>
                      ))
                    )}
                  </div>

                  <pre className="hub-json-preview" id="effective-mcp-json">
                    {JSON.stringify(effective.mcp_client_config, null, 2)}
                  </pre>
                </>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
