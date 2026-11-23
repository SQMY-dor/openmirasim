import { AlertCircle, Moon, Play, Server, Sun } from "lucide-react";
import { useState, type FormEvent } from "react";

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

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault();
    if (!serverUrl.trim() || !token.trim()) return;
    void onConnectServer(serverUrl.trim(), token.trim());
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
          连接到团队的 Linux 中心服务器（<code>om-server</code>），使用静态 Token
          接入实时事件流与持久化会话。
        </p>

        {error && (
          <div className="error-banner" role="alert" id="connect-error">
            <AlertCircle size={15} style={{ flexShrink: 0, marginTop: 2 }} />
            <span>{error}</span>
          </div>
        )}

        <form onSubmit={handleSubmit}>
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
            <label htmlFor="access-token-input">访问令牌（Bearer Token）</label>
            <input
              id="access-token-input"
              className="input input-mono"
              type="password"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder="输入静态访问 Token"
              autoComplete="current-password"
              required
            />
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
