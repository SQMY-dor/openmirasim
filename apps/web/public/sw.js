/* OpenMirasim PWA Service Worker (W2-4): offline shell fallback + interactive Approval Web Push */

const CACHE_NAME = "openmirasim-pwa-v1";
const APP_SHELL = ["/", "/manifest.webmanifest", "/icons/icon.svg"];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE_NAME)
      .then((cache) => cache.addAll(APP_SHELL))
      .catch(() => undefined)
      .then(() => self.skipWaiting())
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(
          keys
            .filter((k) => k !== CACHE_NAME)
            .map((k) => caches.delete(k))
        )
      )
      .then(() => self.clients.claim())
  );
});

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;
  const url = new URL(req.url);
  if (url.pathname.startsWith("/api/") || url.pathname.startsWith("/ws/")) {
    return;
  }
  if (req.mode === "navigate") {
    event.respondWith(
      fetch(req).catch(() =>
        caches.match("/").then((cached) => cached || Response.error())
      )
    );
  }
});

self.addEventListener("push", (event) => {
  let data = {
    title: "OpenMirasim 审批提醒",
    body: "有 Agent 工具调用等待您的确认。",
    approvalId: "",
    sessionId: "",
    allowOptionId: "allow_once",
    rejectOptionId: "reject_once",
  };
  if (event.data) {
    try {
      const parsed = event.data.json();
      data = { ...data, ...parsed };
    } catch {
      data.body = event.data.text();
    }
  }

  const options = {
    body: data.body,
    icon: "/icons/icon.svg",
    badge: "/icons/icon.svg",
    tag: data.approvalId ? `approval-${data.approvalId}` : "openmirasim-approval",
    renotify: true,
    requireInteraction: true,
    data,
    actions: data.approvalId
      ? [
          { action: "allow", title: "允许一次" },
          { action: "reject", title: "拒绝" },
        ]
      : [],
  };

  event.waitUntil(self.registration.showNotification(data.title, options));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const data = event.notification.data || {};
  const action = event.action || "";
  const optionId =
    action === "allow"
      ? data.allowOptionId || "allow_once"
      : action === "reject"
        ? data.rejectOptionId || "reject_once"
        : "";

  event.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then((clients) => {
        if (clients.length > 0) {
          const target = clients[0];
          if (optionId && data.approvalId) {
            target.postMessage({
              type: "om_approval_action",
              approvalId: data.approvalId,
              sessionId: data.sessionId || "",
              optionId,
            });
          } else if (data.sessionId) {
            target.postMessage({
              type: "om_focus_session",
              sessionId: data.sessionId,
            });
          }
          return target.focus();
        }
        const params = new URLSearchParams();
        if (data.sessionId) params.set("session", data.sessionId);
        if (data.approvalId && optionId) {
          params.set("approve", data.approvalId);
          params.set("option", optionId);
        }
        const qs = params.toString();
        return self.clients.openWindow(qs ? `/?${qs}` : "/");
      })
  );
});
