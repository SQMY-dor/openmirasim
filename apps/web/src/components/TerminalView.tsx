import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { Download, Play, RotateCcw, Terminal as TermIcon } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { OpenMirasimClient } from "../protocol/client";
import type { SessionId } from "../protocol/types";

interface TerminalViewProps {
  sessionId: SessionId;
  client: OpenMirasimClient;
  theme: "dark" | "light";
  writable: boolean;
}

type PtyConnState = "connecting" | "connected" | "readonly" | "exited" | "error";

interface PtyControlFrame {
  type: "ready" | "lagged" | "exited" | "pong" | "error";
  cols?: number;
  rows?: number;
  writable?: boolean;
  recording?: boolean;
  skipped?: number;
  code?: number | null;
  message?: string;
}

function terminalTheme(mode: "dark" | "light") {
  if (mode === "light") {
    return {
      background: "#f8fafc",
      foreground: "#0f172a",
      cursor: "#0f766e",
      selectionBackground: "rgba(15, 118, 110, 0.22)",
    };
  }
  return {
    background: "#0b0e14",
    foreground: "#e6edf3",
    cursor: "#6cd7ba",
    selectionBackground: "rgba(108, 215, 186, 0.25)",
  };
}

export function TerminalView({
  sessionId,
  client,
  theme,
  writable,
}: TerminalViewProps) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const replayAbortRef = useRef(false);

  const [connState, setConnState] = useState<PtyConnState>("connecting");
  const [dims, setDims] = useState<{ cols: number; rows: number }>({
    cols: 100,
    rows: 30,
  });
  const [statusNote, setStatusNote] = useState<string | null>(null);
  const [replaying, setReplaying] = useState(false);
  const [epoch, setEpoch] = useState(0);

  useEffect(() => {
    if (termRef.current) {
      termRef.current.options.theme = terminalTheme(theme);
    }
  }, [theme]);

  useEffect(() => {
    const container = hostRef.current;
    if (!container) return;

    replayAbortRef.current = true;
    setReplaying(false);
    setStatusNote(null);

    const term = new Terminal({
      cursorBlink: writable,
      disableStdin: !writable,
      convertEol: false,
      fontFamily: '"IBM Plex Mono", ui-monospace, SFMono-Regular, Menlo, monospace',
      fontSize: 13,
      lineHeight: 1.32,
      scrollback: 5000,
      theme: terminalTheme(theme),
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(container);
    try {
      fit.fit();
    } catch {
      // ignore initial layout measurement race
    }
    termRef.current = term;
    fitRef.current = fit;

    const initialCols = Math.max(40, term.cols || 100);
    const initialRows = Math.max(12, term.rows || 30);
    setDims({ cols: initialCols, rows: initialRows });

    const encoder = new TextEncoder();
    const decoder = new TextDecoder();
    const wsUrl = client.buildPtyWsUrl(sessionId, initialCols, initialRows);

    if (!wsUrl) {
      // Local Mock demo terminal fallback
      setConnState(writable ? "connected" : "readonly");
      term.writeln(
        "\x1b[36mOpenMirasim Mock PTY (xterm.js)\x1b[0m — 连接真实 om-server 后将直连 /ws/pty Binary 流。"
      );
      term.write("openmirasim$ ");
      let lineBuf = "";
      const disposable = term.onData((data) => {
        if (!writable) return;
        for (const ch of data) {
          if (ch === "\r") {
            term.write("\r\n");
            if (lineBuf.trim()) {
              term.writeln(`mock-pty: ${lineBuf.trim()}`);
            }
            lineBuf = "";
            term.write("openmirasim$ ");
          } else if (ch === "\x7f") {
            if (lineBuf.length > 0) {
              lineBuf = lineBuf.slice(0, -1);
              term.write("\b \b");
            }
          } else {
            lineBuf += ch;
            term.write(ch);
          }
        }
      });

      const ro = new ResizeObserver(() => {
        try {
          fit.fit();
          setDims({ cols: term.cols, rows: term.rows });
        } catch {
          // ignore
        }
      });
      ro.observe(container);

      return () => {
        ro.disconnect();
        disposable.dispose();
        term.dispose();
        termRef.current = null;
        fitRef.current = null;
      };
    }

    setConnState("connecting");
    const ws = new WebSocket(wsUrl);
    ws.binaryType = "arraybuffer";
    wsRef.current = ws;

    const sendResize = (cols: number, rows: number) => {
      setDims({ cols, rows });
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "resize", cols, rows }));
      }
    };

    const dataDisposable = term.onData((chunk) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(encoder.encode(chunk));
      }
    });

    ws.onopen = () => {
      try {
        fit.fit();
        sendResize(term.cols, term.rows);
      } catch {
        // ignore
      }
    };

    ws.onmessage = (ev) => {
      if (ev.data instanceof ArrayBuffer) {
        const bytes = new Uint8Array(ev.data);
        term.write(decoder.decode(bytes, { stream: true }));
        return;
      }
      if (typeof ev.data === "string") {
        try {
          const ctrl = JSON.parse(ev.data) as PtyControlFrame;
          if (ctrl.type === "ready") {
            setConnState(ctrl.writable ? "connected" : "readonly");
            if (ctrl.cols && ctrl.rows) {
              setDims({ cols: ctrl.cols, rows: ctrl.rows });
            }
          } else if (ctrl.type === "lagged") {
            setStatusNote(`终端输出较快，已跳过 ${ctrl.skipped ?? 0} 个积压块`);
          } else if (ctrl.type === "exited") {
            setConnState("exited");
            term.writeln(
              `\r\n\x1b[33m[PTY 进程已退出 code=${ctrl.code ?? "unknown"}]\x1b[0m`
            );
          } else if (ctrl.type === "error" && ctrl.message) {
            setStatusNote(ctrl.message);
          }
        } catch {
          // ignore malformed control frames
        }
      }
    };

    ws.onerror = () => {
      setConnState("error");
      setStatusNote("PTY WebSocket 连接异常");
    };

    ws.onclose = () => {
      setConnState((prev) => (prev === "error" ? "error" : "exited"));
    };

    const ro = new ResizeObserver(() => {
      try {
        fit.fit();
        sendResize(term.cols, term.rows);
      } catch {
        // ignore
      }
    });
    ro.observe(container);

    return () => {
      replayAbortRef.current = true;
      ro.disconnect();
      dataDisposable.dispose();
      ws.onopen = null;
      ws.onmessage = null;
      ws.onerror = null;
      ws.onclose = null;
      ws.close();
      wsRef.current = null;
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, client, epoch]);

  const handleRestartPty = useCallback(async () => {
    setStatusNote(null);
    try {
      await client.killPty(sessionId);
    } catch {
      // ignore if already stopped
    }
    setEpoch((n) => n + 1);
  }, [client, sessionId]);

  const handleReplayCast = useCallback(async () => {
    const term = termRef.current;
    if (!term || replaying) return;
    setReplaying(true);
    replayAbortRef.current = false;
    setStatusNote("正在加载 asciicast v2 (.cast) 录制…");
    try {
      const raw = await client.fetchPtyRecording(sessionId);
      const lines = raw.split("\n").filter((l) => l.trim().length > 0);
      if (lines.length === 0) {
        setStatusNote("录制文件为空");
        return;
      }
      term.clear();
      term.writeln("\x1b[36m--- 开始回放 asciicast v2 (.cast) ---\x1b[0m");
      let prevTs = 0;
      for (let i = 1; i < lines.length; i += 1) {
        if (replayAbortRef.current) break;
        try {
          const entry = JSON.parse(lines[i]) as [number, string, string];
          if (Array.isArray(entry) && entry[1] === "o" && typeof entry[2] === "string") {
            const delayMs = Math.min(120, Math.max(0, (entry[0] - prevTs) * 250));
            prevTs = entry[0];
            if (delayMs > 4) {
              await new Promise((r) => window.setTimeout(r, delayMs));
            }
            term.write(entry[2]);
          }
        } catch {
          // ignore malformed line
        }
      }
      if (!replayAbortRef.current) {
        term.writeln("\r\n\x1b[36m--- 回放结束 ---\x1b[0m");
        setStatusNote("asciicast v2 回放完成");
      }
    } catch (err) {
      setStatusNote(err instanceof Error ? err.message : "加载 .cast 录制失败");
    } finally {
      setReplaying(false);
    }
  }, [client, sessionId, replaying]);

  const handleDownloadCast = useCallback(async () => {
    try {
      const raw = await client.fetchPtyRecording(sessionId);
      const blob = new Blob([raw], { type: "application/x-asciicast" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${sessionId}.cast`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      setStatusNote(err instanceof Error ? err.message : "下载 .cast 失败");
    }
  }, [client, sessionId]);

  const stateChip = (() => {
    switch (connState) {
      case "connected":
        return { label: "PTY 已连接", cls: "ok" };
      case "readonly":
        return { label: "PTY 只读 (Viewer)", cls: "warn" };
      case "connecting":
        return { label: "连接 PTY…", cls: "warn" };
      case "error":
        return { label: "PTY 错误", cls: "err" };
      default:
        return { label: "PTY 已退出", cls: "" };
    }
  })();

  return (
    <section className="terminal-stage" id="pty-terminal-view" aria-label="原生 PTY 终端">
      <div className="terminal-toolbar">
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <TermIcon size={14} />
          <span className={`chip ${stateChip.cls}`} id="pty-status-chip">
            {stateChip.label}
          </span>
          <span className="chip">
            {dims.cols}×{dims.rows}
          </span>
          {statusNote && (
            <span className="terminal-note" id="pty-status-note">
              {statusNote}
            </span>
          )}
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <button
            type="button"
            id="replay-cast-btn"
            className="btn btn-sm"
            disabled={replaying}
            onClick={() => void handleReplayCast()}
            title="在当前终端内回放 asciicast v2 (.cast) 录制"
          >
            <Play size={12} />
            <span>{replaying ? "回放中…" : "回放 .cast"}</span>
          </button>
          <button
            type="button"
            id="download-cast-btn"
            className="btn btn-sm"
            onClick={() => void handleDownloadCast()}
            title="下载 asciicast v2 (.cast) 录制文件"
          >
            <Download size={12} />
            <span>下载 .cast</span>
          </button>
          {writable && (
            <button
              type="button"
              id="restart-pty-btn"
              className="btn btn-sm"
              onClick={() => void handleRestartPty()}
              title="终止并重新启动该会话的原生 Shell PTY"
            >
              <RotateCcw size={12} />
              <span>重启终端</span>
            </button>
          )}
        </div>
      </div>

      <div className="terminal-canvas" ref={hostRef} />
    </section>
  );
}
