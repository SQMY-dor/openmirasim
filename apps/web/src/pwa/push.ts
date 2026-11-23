/**
 * PWA Service Worker registration & Approval Push Notification bridge (W2-4).
 */

import type { OpenMirasimClient } from "../protocol/client";
import type { ApprovalRecord, SessionId } from "../protocol/types";

export interface ApprovalNotificationAction {
  approvalId: string;
  sessionId: string;
  optionId: string;
}

const PUSH_ENABLED_KEY = "openmirasim.push_alerts.v1";

export function readPushPreference(): boolean {
  try {
    return window.localStorage.getItem(PUSH_ENABLED_KEY) === "1";
  } catch {
    return false;
  }
}

export function savePushPreference(enabled: boolean): void {
  try {
    window.localStorage.setItem(PUSH_ENABLED_KEY, enabled ? "1" : "0");
  } catch {
    // ignore storage errors
  }
}

function urlBase64ToUint8Array(base64String: string): Uint8Array<ArrayBuffer> {
  const padding = "=".repeat((4 - (base64String.length % 4)) % 4);
  const base64 = (base64String + padding).replace(/-/g, "+").replace(/_/g, "/");
  const raw = window.atob(base64);
  const buffer = new ArrayBuffer(raw.length);
  const output = new Uint8Array(buffer);
  for (let i = 0; i < raw.length; i += 1) {
    output[i] = raw.charCodeAt(i);
  }
  return output;
}

function arrayBufferToBase64Url(buf: ArrayBuffer | null): string {
  if (!buf) return "";
  const bytes = new Uint8Array(buf);
  let binary = "";
  for (const b of bytes) {
    binary += String.fromCharCode(b);
  }
  return window
    .btoa(binary)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

export async function registerServiceWorker(): Promise<ServiceWorkerRegistration | null> {
  if (typeof window === "undefined" || !("serviceWorker" in navigator)) {
    return null;
  }
  try {
    return await navigator.serviceWorker.register("/sw.js", { scope: "/" });
  } catch {
    return null;
  }
}

/**
 * Enables browser / PWA approval notifications and, when a VAPID key is advertised by the
 * server, registers a real PushManager subscription via `POST /api/v1/push/subscriptions`.
 */
export async function enableApprovalPush(
  client: OpenMirasimClient
): Promise<{ ok: boolean; detail: string }> {
  if (typeof window === "undefined" || !("Notification" in window)) {
    return { ok: false, detail: "当前浏览器不支持系统通知 API" };
  }

  let perm = Notification.permission;
  if (perm === "default") {
    perm = await Notification.requestPermission();
  }
  if (perm !== "granted") {
    return { ok: false, detail: "浏览器通知权限未授予" };
  }

  savePushPreference(true);
  const reg = await registerServiceWorker();

  try {
    const cfg = await client.getPushConfig();
    if (reg && "pushManager" in reg && cfg.vapid_public_key) {
      let sub = await reg.pushManager.getSubscription();
      if (!sub) {
        sub = await reg.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: urlBase64ToUint8Array(cfg.vapid_public_key),
        });
      }
      const p256dh = arrayBufferToBase64Url(sub.getKey("p256dh"));
      const auth = arrayBufferToBase64Url(sub.getKey("auth"));
      if (sub.endpoint && p256dh && auth) {
        await client.registerPushSubscription({
          endpoint: sub.endpoint,
          keys: { p256dh, auth },
          user_agent: navigator.userAgent,
        });
        return { ok: true, detail: "已启用 Web Push 远程审批推送" };
      }
    }
  } catch {
    // Fallback cleanly to local Service Worker notifications (e.g. HTTP localhost without VAPID)
  }

  return { ok: true, detail: "已启用 PWA Service Worker 审批通知" };
}

/**
 * Displays an interactive Approval Notification (with Allow Once / Reject buttons)
 * through the active Service Worker registration when approval alerts are enabled.
 */
export async function notifyPendingApproval(
  sessionId: SessionId,
  approval: ApprovalRecord
): Promise<void> {
  if (
    typeof window === "undefined" ||
    !("Notification" in window) ||
    Notification.permission !== "granted" ||
    !readPushPreference()
  ) {
    return;
  }

  const allowOpt = approval.options.find((o) => o.kind === "allow_once") ??
    approval.options.find((o) => o.kind === "allow_always");
  const rejectOpt = approval.options.find((o) => o.kind === "reject_once") ??
    approval.options.find((o) => o.kind === "reject_always");

  const title = "OpenMirasim 审批请求";
  const firstLocation = approval.locations?.[0];
  const body = `${approval.title}${firstLocation ? ` (${firstLocation})` : ""}`;
  const data = {
    approvalId: approval.id,
    sessionId,
    allowOptionId: allowOpt?.option_id ?? "allow_once",
    rejectOptionId: rejectOpt?.option_id ?? "reject_once",
  };

  try {
    const reg =
      "serviceWorker" in navigator
        ? await navigator.serviceWorker.getRegistration()
        : undefined;
    if (reg) {
      await reg.showNotification(title, {
        body,
        icon: "/icons/icon.svg",
        badge: "/icons/icon.svg",
        tag: `approval-${approval.id}`,
        requireInteraction: true,
        data,
        ...(allowOpt && rejectOpt
          ? {
              actions: [
                { action: "allow", title: allowOpt.name || "允许一次" },
                { action: "reject", title: rejectOpt.name || "拒绝" },
              ],
            }
          : {}),
      } as NotificationOptions);
      return;
    }
    new Notification(title, { body, icon: "/icons/icon.svg", tag: `approval-${approval.id}` });
  } catch {
    // Ignore environments restricting notifications
  }
}

/**
 * Subscribes to `postMessage` events sent from `/sw.js` when the user clicks
 * an Approval Notification action button.
 */
export function subscribeServiceWorkerActions(handlers: {
  onApprovalAction: (action: ApprovalNotificationAction) => void;
  onFocusSession: (sessionId: string) => void;
}): () => void {
  if (typeof window === "undefined" || !("serviceWorker" in navigator)) {
    return () => {};
  }
  const listener = (event: MessageEvent) => {
    const payload = event.data as Record<string, unknown> | null;
    if (!payload || typeof payload !== "object") return;
    if (
      payload.type === "om_approval_action" &&
      typeof payload.approvalId === "string" &&
      typeof payload.optionId === "string"
    ) {
      handlers.onApprovalAction({
        approvalId: payload.approvalId,
        sessionId: typeof payload.sessionId === "string" ? payload.sessionId : "",
        optionId: payload.optionId,
      });
    } else if (
      payload.type === "om_focus_session" &&
      typeof payload.sessionId === "string"
    ) {
      handlers.onFocusSession(payload.sessionId);
    }
  };
  navigator.serviceWorker.addEventListener("message", listener);
  return () => {
    navigator.serviceWorker.removeEventListener("message", listener);
  };
}
