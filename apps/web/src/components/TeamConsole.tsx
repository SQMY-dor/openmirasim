import {
  Bell,
  Check,
  Copy,
  KeyRound,
  RefreshCw,
  ScrollText,
  Shield,
  Trash2,
  UserPlus,
  Users,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import type { OpenMirasimClient } from "../protocol/client";
import {
  MAX_MEMBERS_PER_SPACE,
  roleCan,
  roleCanManage,
  type AuditEntry,
  type DeviceTokenRecord,
  type IssuedToken,
  type MemberId,
  type MemberRecord,
  type Principal,
  type ProviderInfo,
  type PushConfigResponse,
  type Role,
} from "../protocol/types";
import { enableApprovalPush, readPushPreference, savePushPreference } from "../pwa/push";

interface TeamConsoleProps {
  open: boolean;
  client: OpenMirasimClient;
  principal: Principal | null;
  onClose: () => void;
  onPushStateChanged?: (enabled: boolean) => void;
}

type TeamTab = "members" | "tokens" | "security";

const ROLE_LABELS: Record<Role, string> = {
  owner: "Owner",
  admin: "Admin",
  member: "Member",
  viewer: "Viewer",
};

const ALL_ROLES: Role[] = ["viewer", "member", "admin", "owner"];

function formatTs(ts: number): string {
  try {
    return new Date(ts).toLocaleString();
  } catch {
    return String(ts);
  }
}

export function TeamConsole({
  open,
  client,
  principal,
  onClose,
  onPushStateChanged,
}: TeamConsoleProps) {
  const [tab, setTab] = useState<TeamTab>("members");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [members, setMembers] = useState<MemberRecord[]>([]);
  const [newName, setNewName] = useState("");
  const [newRole, setNewRole] = useState<Role>("member");

  const [selectedMemberId, setSelectedMemberId] = useState<MemberId>("");
  const [tokens, setTokens] = useState<DeviceTokenRecord[]>([]);
  const [tokenLabel, setTokenLabel] = useState("");
  const [tokenTtlDays, setTokenTtlDays] = useState("30");
  const [freshToken, setFreshToken] = useState<IssuedToken | null>(null);
  const [copiedSecret, setCopiedSecret] = useState(false);

  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [auditEntries, setAuditEntries] = useState<AuditEntry[]>([]);
  const [auditFilter, setAuditFilter] = useState("");
  const [pushCfg, setPushCfg] = useState<PushConfigResponse | null>(null);
  const [pushEnabled, setPushEnabled] = useState(readPushPreference);
  const [pushNote, setPushNote] = useState<string | null>(null);

  const myRole: Role = principal?.role ?? "viewer";
  const canManageMembers = roleCan(myRole, "manage_members");
  const canViewAudit = roleCan(myRole, "view_audit");

  const assignableRoles = useMemo(
    () => ALL_ROLES.filter((r) => roleCanManage(myRole, r)),
    [myRole]
  );

  const activeMemberCount = useMemo(
    () => members.filter((m) => !m.disabled_at).length,
    [members]
  );

  const loadMembers = useCallback(async () => {
    const list = await client.listMembers();
    setMembers(list);
    setSelectedMemberId((prev) => {
      if (prev && list.some((m) => m.id === prev)) return prev;
      return principal?.member_id ?? list[0]?.id ?? "";
    });
  }, [client, principal?.member_id]);

  const loadTokens = useCallback(
    async (memberId: MemberId) => {
      if (!memberId) return;
      const list = await client.listMemberTokens(memberId);
      setTokens(list);
    },
    [client]
  );

  const loadSecurity = useCallback(async () => {
    const [provs, push] = await Promise.all([
      client.listProviders().catch(() => []),
      client.getPushConfig().catch(() => null),
    ]);
    setProviders(provs);
    setPushCfg(push);
    if (canViewAudit) {
      const entries = await client.listAudit(auditFilter || undefined, 80);
      setAuditEntries(entries);
    } else {
      setAuditEntries([]);
    }
  }, [client, canViewAudit, auditFilter]);

  const refreshActiveTab = useCallback(async () => {
    if (!open) return;
    setLoading(true);
    setError(null);
    try {
      if (tab === "members") {
        await loadMembers();
      } else if (tab === "tokens") {
        if (members.length === 0) {
          await loadMembers();
        }
        const target = selectedMemberId || principal?.member_id || "";
        if (target) {
          await loadTokens(target);
        }
      } else {
        await loadSecurity();
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "加载团队配置失败");
    } finally {
      setLoading(false);
    }
  }, [
    open,
    tab,
    loadMembers,
    loadTokens,
    loadSecurity,
    members.length,
    selectedMemberId,
    principal?.member_id,
  ]);

  useEffect(() => {
    if (!open) return;
    void refreshActiveTab();
  }, [open, tab, refreshActiveTab]);

  useEffect(() => {
    if (!open || tab !== "tokens" || !selectedMemberId) return;
    void loadTokens(selectedMemberId).catch((err) => {
      setError(err instanceof Error ? err.message : "加载设备 Token 失败");
    });
  }, [open, tab, selectedMemberId, loadTokens]);

  if (!open) return null;

  const handleAddMember = async () => {
    if (!newName.trim()) return;
    setLoading(true);
    setError(null);
    try {
      const created = await client.addMember(newName.trim(), newRole);
      setNewName("");
      await loadMembers();
      setSelectedMemberId(created.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : "添加成员失败");
    } finally {
      setLoading(false);
    }
  };

  const handleChangeRole = async (memberId: MemberId, role: Role) => {
    setLoading(true);
    setError(null);
    try {
      await client.changeMemberRole(memberId, role);
      await loadMembers();
    } catch (err) {
      setError(err instanceof Error ? err.message : "变更角色失败");
    } finally {
      setLoading(false);
    }
  };

  const handleDisableMember = async (memberId: MemberId) => {
    setLoading(true);
    setError(null);
    try {
      await client.disableMember(memberId);
      await loadMembers();
    } catch (err) {
      setError(err instanceof Error ? err.message : "停用成员失败");
    } finally {
      setLoading(false);
    }
  };

  const handleIssueToken = async () => {
    const target = selectedMemberId || principal?.member_id;
    if (!target || !tokenLabel.trim()) return;
    setLoading(true);
    setError(null);
    setFreshToken(null);
    setCopiedSecret(false);
    try {
      const days = Number(tokenTtlDays);
      const ttlSecs =
        Number.isFinite(days) && days > 0 ? Math.round(days * 86_400) : undefined;
      const issued = await client.issueMemberToken(
        target,
        tokenLabel.trim(),
        ttlSecs
      );
      setFreshToken(issued);
      setTokenLabel("");
      await loadTokens(target);
    } catch (err) {
      setError(err instanceof Error ? err.message : "签发设备 Token 失败");
    } finally {
      setLoading(false);
    }
  };

  const handleRevokeToken = async (tokenId: string) => {
    setLoading(true);
    setError(null);
    try {
      await client.revokeToken(tokenId);
      if (selectedMemberId) {
        await loadTokens(selectedMemberId);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "吊销 Token 失败");
    } finally {
      setLoading(false);
    }
  };

  const handleCopySecret = async () => {
    if (!freshToken) return;
    try {
      await navigator.clipboard.writeText(freshToken.secret);
      setCopiedSecret(true);
    } catch {
      // ignore clipboard restrictions
    }
  };

  const handleTogglePush = async () => {
    setPushNote(null);
    if (pushEnabled) {
      savePushPreference(false);
      setPushEnabled(false);
      onPushStateChanged?.(false);
      setPushNote("已关闭本地 PWA 审批通知");
      return;
    }
    const res = await enableApprovalPush(client);
    setPushEnabled(res.ok);
    onPushStateChanged?.(res.ok);
    setPushNote(res.detail);
    await loadSecurity().catch(() => {});
  };

  const manageableMembers = members.filter(
    (m) =>
      m.id === principal?.member_id ||
      (canManageMembers && roleCanManage(myRole, m.role))
  );

  return (
    <div
      className="modal-backdrop"
      role="dialog"
      aria-modal="true"
      aria-labelledby="team-console-title"
      onClick={onClose}
    >
      <div
        className="modal-card team-modal"
        id="team-console-modal"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="modal-head">
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <Shield size={16} />
            <h2 id="team-console-title">团队权限 · 设备 Token · 安全审计</h2>
            {principal && (
              <span className="chip ok">
                {principal.display_name} · {ROLE_LABELS[principal.role]}
              </span>
            )}
          </div>
          <div style={{ display: "flex", gap: 4 }}>
            <button
              type="button"
              className="icon-btn"
              onClick={() => void refreshActiveTab()}
              aria-label="刷新"
            >
              <RefreshCw size={14} />
            </button>
            <button
              type="button"
              className="icon-btn"
              onClick={onClose}
              aria-label="关闭团队控制台"
            >
              <X size={16} />
            </button>
          </div>
        </div>

        <div className="drawer-tabs" style={{ marginBottom: 14 }}>
          <button
            type="button"
            id="tab-team-members"
            className={`drawer-tab ${tab === "members" ? "active" : ""}`}
            onClick={() => setTab("members")}
          >
            <Users size={13} />
            <span>
              成员 ({activeMemberCount}/{MAX_MEMBERS_PER_SPACE})
            </span>
          </button>
          <button
            type="button"
            id="tab-team-tokens"
            className={`drawer-tab ${tab === "tokens" ? "active" : ""}`}
            onClick={() => setTab("tokens")}
          >
            <KeyRound size={13} />
            <span>设备 Token</span>
          </button>
          <button
            type="button"
            id="tab-team-security"
            className={`drawer-tab ${tab === "security" ? "active" : ""}`}
            onClick={() => setTab("security")}
          >
            <ScrollText size={13} />
            <span>密钥代理 · 推送 · 审计</span>
          </button>
        </div>

        {error && (
          <div className="error-banner" role="alert" style={{ marginBottom: 10 }}>
            <span>{error}</span>
          </div>
        )}

        <div className="team-console-body">
          {tab === "members" && (
            <div className="drawer-section">
              {canManageMembers && (
                <div className="inline-register-box" style={{ marginBottom: 12 }}>
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 6,
                      fontSize: 12.5,
                      fontWeight: 600,
                      marginBottom: 8,
                    }}
                  >
                    <UserPlus size={14} />
                    <span>添加团队成员（单空间上限 {MAX_MEMBERS_PER_SPACE} 人）</span>
                  </div>
                  <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
                    <input
                      id="add-member-name"
                      className="input"
                      style={{ flex: "1 1 180px" }}
                      type="text"
                      placeholder="成员显示名（如 Alice）"
                      value={newName}
                      onChange={(e) => setNewName(e.target.value)}
                    />
                    <select
                      id="add-member-role"
                      className="select"
                      style={{ width: 130 }}
                      value={newRole}
                      onChange={(e) => setNewRole(e.target.value as Role)}
                    >
                      {assignableRoles.map((r) => (
                        <option key={r} value={r}>
                          {ROLE_LABELS[r]}
                        </option>
                      ))}
                    </select>
                    <button
                      type="button"
                      id="confirm-add-member-btn"
                      className="btn btn-primary btn-sm"
                      disabled={loading || !newName.trim()}
                      onClick={() => void handleAddMember()}
                    >
                      添加成员
                    </button>
                  </div>
                </div>
              )}

              <div className="team-list" id="team-members-list">
                {members.map((m) => {
                  const isSelf = m.id === principal?.member_id;
                  const canEdit =
                    !m.disabled_at &&
                    !isSelf &&
                    canManageMembers &&
                    roleCanManage(myRole, m.role);
                  const canOpenTokens = isSelf || (canManageMembers && roleCanManage(myRole, m.role));

                  return (
                    <div
                      key={m.id}
                      className={`team-row ${m.disabled_at ? "disabled" : ""}`}
                      data-member-id={m.id}
                    >
                      <div className="team-row-main">
                        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                          <strong>{m.display_name}</strong>
                          <span className={`chip ${m.role === "owner" ? "ok" : ""}`}>
                            {ROLE_LABELS[m.role]}
                          </span>
                          {isSelf && <span className="chip">当前登录</span>}
                          {m.disabled_at && <span className="chip err">已停用</span>}
                        </div>
                        <div className="team-row-sub">
                          ID: {m.id} · 加入于 {formatTs(m.created_at)}
                        </div>
                      </div>

                      <div className="team-row-actions">
                        {canOpenTokens && !m.disabled_at && (
                          <button
                            type="button"
                            className="btn btn-sm"
                            onClick={() => {
                              setSelectedMemberId(m.id);
                              setFreshToken(null);
                              setTab("tokens");
                            }}
                          >
                            <KeyRound size={12} />
                            <span>Token</span>
                          </button>
                        )}
                        {canEdit && (
                          <>
                            <select
                              aria-label={`变更 ${m.display_name} 角色`}
                              className="select select-sm"
                              value={m.role}
                              onChange={(e) =>
                                void handleChangeRole(m.id, e.target.value as Role)
                              }
                            >
                              {assignableRoles.map((r) => (
                                <option key={r} value={r}>
                                  {ROLE_LABELS[r]}
                                </option>
                              ))}
                            </select>
                            <button
                              type="button"
                              className="btn btn-sm btn-danger"
                              title="停用成员并立即吊销所有设备 Token"
                              onClick={() => void handleDisableMember(m.id)}
                            >
                              <Trash2 size={12} />
                            </button>
                          </>
                        )}
                      </div>
                    </div>
                  );
                })}
              </div>
            </div>
          )}

          {tab === "tokens" && (
            <div className="drawer-section">
              <div className="inline-register-box" style={{ marginBottom: 12 }}>
                <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
                  <select
                    id="token-member-select"
                    className="select"
                    style={{ width: 180 }}
                    value={selectedMemberId}
                    onChange={(e) => {
                      setSelectedMemberId(e.target.value);
                      setFreshToken(null);
                    }}
                  >
                    {manageableMembers.map((m) => (
                      <option key={m.id} value={m.id}>
                        {m.display_name} ({ROLE_LABELS[m.role]})
                      </option>
                    ))}
                  </select>
                  <input
                    id="issue-token-label"
                    className="input"
                    style={{ flex: "1 1 160px" }}
                    type="text"
                    placeholder="设备标签（如 MacBook / iPhone PWA）"
                    value={tokenLabel}
                    onChange={(e) => setTokenLabel(e.target.value)}
                  />
                  <input
                    id="issue-token-ttl"
                    className="input"
                    style={{ width: 96 }}
                    type="number"
                    min="0"
                    placeholder="天数"
                    title="有效期天数（留空或 0 表示不过期）"
                    value={tokenTtlDays}
                    onChange={(e) => setTokenTtlDays(e.target.value)}
                  />
                  <button
                    type="button"
                    id="confirm-issue-token-btn"
                    className="btn btn-primary btn-sm"
                    disabled={loading || !selectedMemberId || !tokenLabel.trim()}
                    onClick={() => void handleIssueToken()}
                  >
                    签发设备 Token
                  </button>
                </div>

                {freshToken && (
                  <div className="token-secret-banner" id="issued-token-banner">
                    <div style={{ fontSize: 12, marginBottom: 4 }}>
                      <strong>新设备 Token 已签发（仅展示一次，请立即保存）：</strong>
                    </div>
                    <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
                      <code id="issued-token-secret" className="token-secret-code">
                        {freshToken.secret}
                      </code>
                      <button
                        type="button"
                        className="btn btn-sm"
                        onClick={() => void handleCopySecret()}
                      >
                        {copiedSecret ? <Check size={12} /> : <Copy size={12} />}
                        <span>{copiedSecret ? "已复制" : "复制"}</span>
                      </button>
                    </div>
                  </div>
                )}
              </div>

              <div className="team-list" id="team-tokens-list">
                {tokens.length === 0 ? (
                  <div style={{ color: "var(--text-muted)", fontSize: 12.5, padding: "8px 0" }}>
                    该成员暂无已签发的设备 Token。
                  </div>
                ) : (
                  tokens.map((t) => {
                    const revoked = Boolean(t.revoked_at);
                    return (
                      <div
                        key={t.id}
                        className={`team-row ${revoked ? "disabled" : ""}`}
                      >
                        <div className="team-row-main">
                          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                            <strong>{t.label}</strong>
                            {revoked ? (
                              <span className="chip err">已吊销</span>
                            ) : (
                              <span className="chip ok">有效</span>
                            )}
                          </div>
                          <div className="team-row-sub">
                            创建 {formatTs(t.created_at)}
                            {t.expires_at ? ` · 过期 ${formatTs(t.expires_at)}` : " · 长期有效"}
                            {t.last_used_at ? ` · 最近使用 ${formatTs(t.last_used_at)}` : ""}
                          </div>
                        </div>
                        {!revoked && (
                          <button
                            type="button"
                            className="btn btn-sm btn-danger"
                            onClick={() => void handleRevokeToken(t.id)}
                          >
                            吊销
                          </button>
                        )}
                      </div>
                    );
                  })
                )}
              </div>
            </div>
          )}

          {tab === "security" && (
            <div className="drawer-section">
              <div className="inline-register-box" style={{ marginBottom: 12 }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 8,
                    flexWrap: "wrap",
                  }}
                >
                  <div>
                    <div style={{ fontWeight: 600, fontSize: 13 }}>
                      宿主机密钥代理（W2-2 KeyProxy）
                    </div>
                    <div className="team-row-sub">
                      真实上游 API Key 仅保留在宿主机代理进程中，沙箱内仅持有短期 ost_ 会话令牌。
                    </div>
                  </div>
                  <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
                    {providers.length === 0 ? (
                      <span className="chip">未启用宿主机代理</span>
                    ) : (
                      providers.map((p) => (
                        <span key={p.id} className="chip ok">
                          {p.id} ({p.kind})
                        </span>
                      ))
                    )}
                  </div>
                </div>
              </div>

              <div className="inline-register-box" style={{ marginBottom: 12 }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 8,
                    flexWrap: "wrap",
                  }}
                >
                  <div>
                    <div style={{ fontWeight: 600, fontSize: 13 }}>
                      PWA 移动端审批推送提醒（W2-4 Web Push）
                    </div>
                    <div className="team-row-sub">
                      已注册设备订阅：{pushCfg?.subscription_count ?? 0} 个
                      {pushCfg?.vapid_public_key ? " · VAPID 已配置" : " · Service Worker 本地通知就绪"}
                    </div>
                  </div>
                  <button
                    type="button"
                    id="enable-push-btn"
                    className={`btn btn-sm ${pushEnabled ? "" : "btn-primary"}`}
                    onClick={() => void handleTogglePush()}
                  >
                    <Bell size={13} />
                    <span>{pushEnabled ? "关闭审批提醒" : "启用审批提醒"}</span>
                  </button>
                </div>
                {pushNote && (
                  <div style={{ fontSize: 12, color: "var(--accent)", marginTop: 6 }}>
                    {pushNote}
                  </div>
                )}
              </div>

              {canViewAudit ? (
                <div>
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      marginBottom: 8,
                      gap: 8,
                    }}
                  >
                    <strong style={{ fontSize: 13 }}>空间安全审计日志</strong>
                    <input
                      className="input"
                      style={{ width: 220 }}
                      type="search"
                      placeholder="按 target ID 过滤…"
                      value={auditFilter}
                      onChange={(e) => setAuditFilter(e.target.value)}
                    />
                  </div>
                  <div className="team-list" id="audit-log-list">
                    {auditEntries.length === 0 ? (
                      <div style={{ color: "var(--text-muted)", fontSize: 12.5 }}>
                        暂无审计记录。
                      </div>
                    ) : (
                      auditEntries.map((entry) => (
                        <div key={entry.id} className="team-row">
                          <div className="team-row-main">
                            <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                              <span className="chip">{entry.action}</span>
                              {entry.target && (
                                <code style={{ fontSize: 11.5 }}>{entry.target}</code>
                              )}
                            </div>
                            <div className="team-row-sub">
                              {formatTs(entry.ts)}
                              {entry.actor ? ` · actor=${entry.actor}` : " · server"}
                            </div>
                          </div>
                        </div>
                      ))
                    )}
                  </div>
                </div>
              ) : (
                <div style={{ color: "var(--text-muted)", fontSize: 12.5 }}>
                  审计日志仅对 Admin / Owner 角色可见。
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
