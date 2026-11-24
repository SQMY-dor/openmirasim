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

function asBoundedString(value, fallback, maxLen = 256) {
  if (typeof value !== "string") return fallback;
  const trimmed = value.trim();
  return trimmed ? trimmed.slice(0, maxLen) : fallback;
}

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin) {
    return;
  }
  if (url.pathname.startsWith("/api/") || url.pathname.startsWith("/ws/")) {
    return;
  }
  if (req.mode === "navigate") {
    event.respondWith(
      fetch(req)
        .then((res) => {
          if (res.ok && res.type === "basic") {
            const copy = res.clone();
            caches
              .open(CACHE_NAME)
              .then((cache) => cache.put("/", copy))
              .catch(() => undefined);
          }
          return res;
        })
        .catch(() =>
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
      if (parsed && typeof parsed === "object") {
        data = {
          title: asBoundedString(parsed.title, data.title, 120),
          body: asBoundedString(parsed.body, data.body, 512),
          approvalId: asBoundedString(parsed.approvalId, "", 128),
          sessionId: asBoundedString(parsed.sessionId, "", 128),
          allowOptionId: asBoundedString(parsed.allowOptionId, "allow_once", 64),
          rejectOptionId: asBoundedString(parsed.rejectOptionId, "reject_once", 64),
        };
      }
    } catch {
      data.body = asBoundedString(event.data.text(), data.body, 512);
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
      ? asBoundedString(data.allowOptionId, "allow_once", 64)
      : action === "reject"
        ? asBoundedString(data.rejectOptionId, "reject_once", 64)
        : "";
  const approvalId = asBoundedString(data.approvalId, "", 128);
  const sessionId = asBoundedString(data.sessionId, "", 128);

  event.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then((clients) => {
        if (clients.length > 0) {
          const target = clients[0];
          if (optionId && approvalId) {
            target.postMessage({
              type: "om_approval_action",
              approvalId,
              sessionId,
              optionId,
            });
          } else if (sessionId) {
            target.postMessage({
              type: "om_focus_session",
              sessionId,
            });
          }
          return target.focus();
        }
        // Never trigger state-changing approvals via URL query parameters (CSRF/clickjacking risk);
        // open the target session and highlight the approval card for explicit confirmation instead.
        const params = new URLSearchParams();
        if (sessionId) params.set("session", sessionId);
        if (approvalId) params.set("focusApproval", approvalId);
        const qs = params.toString();
        return self.clients.openWindow(qs ? `/?${qs}` : "/");
      })
  );
});
