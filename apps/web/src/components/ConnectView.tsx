import {
  AlertCircle,
  BookmarkPlus,
  KeyRound,
  Moon,
  Play,
  Server,
  Sun,
  Trash2,
} from "lucide-react";
import { useEffect, useState, type FormEvent } from "react";
import {
  deleteServerProfile,
  isTauriDesktop,
  listServerProfiles,
  loadProfileTokenFromKeychain,
  saveProfileTokenToKeychain,
  saveServerProfile,
} from "../desktop/tauriBridge";
import type { ServerProfile } from "../protocol/types";

interface ConnectViewProps {
  initialServerUrl: string;
  initialToken: string;
  theme: "dark" | "light";
  connecting: boolean;
  error: string | null;
  onToggleTheme: () => void;
  onConnectServer: (serverUrl: string, token: string) => Promise<void>;
  onEnterDemo: () => Promise<void>;
}

export function ConnectView({
  initialServerUrl,
  initialToken,
  theme,
  connecting,
  error,
  onToggleTheme,
  onConnectServer,
  onEnterDemo,
}: ConnectViewProps) {
  const [serverUrl, setServerUrl] = useState(initialServerUrl);
  const [token, setToken] = useState(initialToken);
  const [profiles, setProfiles] = useState<ServerProfile[]>([]);
  const [profileName, setProfileName] = useState("");
  const desktopMode = isTauriDesktop();

  useEffect(() => {
    void listServerProfiles().then(setProfiles);
  }, []);

  const handleSelectProfile = async (profile: ServerProfile) => {
    setServerUrl(profile.serverUrl);
    setProfileName(profile.name);
    const savedSecret = await loadProfileTokenFromKeychain(profile.id);
    if (savedSecret) {
      setToken(savedSecret);
    }
  };

  const handleSaveProfile = async () => {
    const cleanUrl = serverUrl.trim();
    if (!cleanUrl) return;
    const id = `prof_${cleanUrl.replace(/[^a-z0-9]/gi, "_").slice(0, 40)}`;
    const name =
      profileName.trim() ||
      cleanUrl.replace(/^https?:\/\//i, "").slice(0, 28);
    const updated = await saveServerProfile({ id, name, serverUrl: cleanUrl });
    setProfiles(updated);
    if (desktopMode && token.trim()) {
      await saveProfileTokenToKeychain(id, token.trim());
    }
  };

  const handleDeleteProfile = async (profileId: string) => {
    const updated = await deleteServerProfile(profileId);
    setProfiles(updated);
  };

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    if (!serverUrl.trim() || !token.trim()) return;
    const cleanUrl = serverUrl.trim();
    const cleanToken = token.trim();
    if (desktopMode) {
      const id = `prof_${cleanUrl.replace(/[^a-z0-9]/gi, "_").slice(0, 40)}`;
      await saveProfileTokenToKeychain(id, cleanToken);
    }
    void onConnectServer(cleanUrl, cleanToken);
  };

  return (
    <main className="connect-shell">
      <section className="connect-card" aria-labelledby="connect-heading">
        <div className="connect-brand">
          <div className="connect-logo">
            <span className="logo-mark" aria-hidden="true">
              OM
            </span>
            <h1 id="connect-heading" className="connect-title">
              OpenMirasim
            </h1>
            {desktopMode && <span className="chip ok">Desktop</span>}
          </div>
          <button
            type="button"
            id="connect-theme-toggle"
            className="icon-btn"
            onClick={onToggleTheme}
            aria-label={theme === "dark" ? "切换到浅色主题" : "切换到深色主题"}
            title={theme === "dark" ? "浅色主题" : "深色主题"}
          >
            {theme === "dark" ? <Sun size={16} /> : <Moon size={16} />}
          </button>
        </div>

        <p className="connect-subtitle">
          连接到团队的 Linux 中心服务器（<code>om-server</code>），使用设备 Token（
          <code>omt_…</code>）或引导 Token 接入实时事件流与原生终端。
        </p>

        {profiles.length > 0 && (
          <div className="profile-bar" id="server-profiles-bar">
            <div className="profile-bar-label">已保存服务器 Profile</div>
            <div className="profile-chips">
              {profiles.map((p) => (
                <div key={p.id} className="profile-chip">
                  <button
                    type="button"
                    className="profile-chip-select"
                    onClick={() => void handleSelectProfile(p)}
                    title={p.serverUrl}
                  >
                    <Server size={11} />
                    <span>{p.name}</span>
                  </button>
                  <button
                    type="button"
                    className="profile-chip-del"
                    aria-label={`删除配置 ${p.name}`}
                    onClick={() => void handleDeleteProfile(p.id)}
                  >
                    <Trash2 size={11} />
                  </button>
                </div>
              ))}
            </div>
          </div>
        )}

        {error && (
          <div className="error-banner" role="alert" id="connect-error">
            <AlertCircle size={15} style={{ flexShrink: 0, marginTop: 2 }} />
            <span>{error}</span>
          </div>
        )}

        <form onSubmit={(e) => void handleSubmit(e)}>
          <div className="field">
            <label htmlFor="server-url-input">中心服务器地址</label>
            <input
              id="server-url-input"
              className="input input-mono"
              type="url"
              value={serverUrl}
              onChange={(e) => setServerUrl(e.target.value)}
              placeholder="http://127.0.0.1:4939"
              required
            />
          </div>

          <div className="field">
            <label htmlFor="access-token-input">
              访问令牌（设备 Token `omt_...` 或静态 Token）
            </label>
            <input
              id="access-token-input"
              className="input input-mono"
              type="password"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder="输入 omt_... 设备 Token 或静态 Token"
              autoComplete="current-password"
              required
            />
            {desktopMode && (
              <div className="keychain-hint">
                <KeyRound size={11} />
                <span>桌面端自动使用系统 Keychain 安全加密保存 Token</span>
              </div>
            )}
          </div>

          <div className="profile-save-row">
            <input
              id="profile-name-input"
              className="input"
              type="text"
              placeholder="可选：保存为服务器配置名（如 Prod Linux）"
              value={profileName}
              onChange={(e) => setProfileName(e.target.value)}
            />
            <button
              type="button"
              id="save-profile-btn"
              className="btn btn-sm"
              disabled={!serverUrl.trim()}
              onClick={() => void handleSaveProfile()}
            >
              <BookmarkPlus size={13} />
              <span>保存配置</span>
            </button>
          </div>

          <div className="connect-actions">
            <button
              id="connect-submit-btn"
              type="submit"
              className="btn btn-primary"
              disabled={connecting || !serverUrl.trim() || !token.trim()}
            >
              <Server size={15} />
              <span>{connecting ? "正在连接…" : "连接中心服务器"}</span>
            </button>

            <div className="connect-divider">或</div>

            <button
              id="enter-demo-btn"
              type="button"
              className="btn"
              disabled={connecting}
              onClick={() => void onEnterDemo()}
            >
              <Play size={14} />
              <span>进入本地 Mock 演示模式</span>
            </button>
          </div>
        </form>
      </section>
    </main>
  );
}
