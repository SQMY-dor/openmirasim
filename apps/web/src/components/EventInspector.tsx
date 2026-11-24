import { Activity, X } from "lucide-react";
import { memo, useMemo } from "react";
import type { SessionEvent, StreamDiagnostics } from "../protocol/types";

interface EventInspectorProps {
  open: boolean;
  diagnostics: StreamDiagnostics;
  rawEvents: SessionEvent[];
  onClose: () => void;
}

function formatTs(tsMs: number): string {
  try {
    return new Date(tsMs).toISOString().slice(11, 19);
  } catch {
    return "";
  }
}

const RawEventRow = memo(function RawEventRow({ ev }: { ev: SessionEvent }) {
  const formatted = useMemo(() => JSON.stringify(ev.payload, null, 2), [ev.payload]);
  return (
    <div className="raw-event">
      <div className="raw-event-head">
        <span>
          #{ev.seq} · {ev.payload.type}
        </span>
        <span style={{ color: "var(--text-muted)" }}>{formatTs(ev.ts)}</span>
      </div>
      <pre className="raw-event-body">{formatted}</pre>
    </div>
  );
});

export function EventInspector({
  open,
  diagnostics,
  rawEvents,
  onClose,
}: EventInspectorProps) {
  if (!open) return null;

  const recentFirst = [...rawEvents].reverse();

  return (
    <aside className="inspector" aria-label="事件流诊断检查器" id="event-inspector">
      <div className="inspector-header">
        <span style={{ display: "inline-flex", alignItems: "center", gap: 7, fontWeight: 600 }}>
          <Activity size={15} />
          <span>Event Stream Inspector</span>
        </span>
        <button
          type="button"
          className="icon-btn"
          onClick={onClose}
          aria-label="关闭检查器"
        >
          <X size={15} />
        </button>
      </div>

      <div className="inspector-stats">
        <div className="stat-cell">
          <div className="stat-label">WS STATE</div>
          <div className="stat-value" id="inspector-ws-state">
            {diagnostics.connectionState}
          </div>
        </div>
        <div className="stat-cell">
          <div className="stat-label">LAST / HEAD SEQ</div>
          <div className="stat-value" id="inspector-last-seq">
            #{diagnostics.lastSeq} / #{diagnostics.headSeq}
          </div>
        </div>
        <div className="stat-cell">
          <div className="stat-label">RECONNECT / RESYNC</div>
          <div className="stat-value" id="inspector-reconnects">
            {diagnostics.reconnectCount} / {diagnostics.resyncCount}
          </div>
        </div>
        <div className="stat-cell">
          <div className="stat-label">DEDUPED / GAPS</div>
          <div className="stat-value">
            {diagnostics.dedupedCount} / {diagnostics.gapDetectedCount}
          </div>
        </div>
      </div>

      <div className="inspector-events">
        {recentFirst.length === 0 ? (
          <div style={{ color: "var(--text-muted)", fontSize: 12, padding: 8 }}>
            暂无已接收的事件帧
          </div>
        ) : (
          recentFirst.map((ev) => (
            <RawEventRow key={`${ev.session_id}:${ev.seq}`} ev={ev} />
          ))
        )}
      </div>
    </aside>
  );
}
