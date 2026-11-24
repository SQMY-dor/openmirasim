/**
 * Runtime bridge between the shared React Web Workbench and the Tauri v2 Desktop Thin Shell
 * (`apps/tauri/`).
 *
 * - Inside Tauri v2: invokes native Rust commands for OS Keychain token storage, multi-server
 *   profile persistence, native desktop notifications, and system tray badge updates.
 * - Inside regular Web browsers: gracefully persists non-secret multi-server bookmarks in
 *   `localStorage` so profile switching works identically on Web & Desktop.
 */

import type { ServerProfile } from "../protocol/types";

const PROFILES_STORAGE_KEY = "openmirasim.server_profiles.v1";

interface TauriInternals {
  invoke: <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;
}

function getTauriInternals(): TauriInternals | null {
  if (typeof window === "undefined") return null;
  const w = window as unknown as { __TAURI_INTERNALS__?: TauriInternals };
  return w.__TAURI_INTERNALS__ && typeof w.__TAURI_INTERNALS__.invoke === "function"
    ? w.__TAURI_INTERNALS__
    : null;
}

export function isTauriDesktop(): boolean {
  return getTauriInternals() !== null;
}

export function normalizeServerUrl(raw: string): string {
  const trimmed = raw.trim().replace(/\/+$/, "");
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    throw new Error("服务器地址无效，请使用完整的 http:// 或 https:// URL");
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error("仅允许 http:// 或 https:// 服务器地址");
  }
  return trimmed;
}

/** Deterministic, collision-free ASCII profile ID derived from the normalized server URL. */
export async function computeProfileId(rawUrl: string): Promise<string> {
  const normalized = rawUrl.trim().replace(/\/+$/, "").toLowerCase();
  if (typeof crypto !== "undefined" && crypto.subtle) {
    try {
      const data = new TextEncoder().encode(normalized);
      const digest = await crypto.subtle.digest("SHA-256", data);
      const hex = Array.from(new Uint8Array(digest))
        .slice(0, 16)
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("");
      return `srv_${hex}`;
    } catch {
      // fall through to deterministic dual-64-bit hash when subtle crypto is unavailable
    }
  }
  let h1 = 0x811c9dc5;
  let h2 = 0x9e3779b9;
  for (let i = 0; i < normalized.length; i += 1) {
    const code = normalized.charCodeAt(i);
    h1 = Math.imul(h1 ^ code, 0x01000193);
    h2 = Math.imul(h2 ^ (code + i), 0x85ebca6b);
  }
  return `srv_${(h1 >>> 0).toString(16).padStart(8, "0")}${(h2 >>> 0).toString(16).padStart(8, "0")}_${normalized.length}`;
}

function readBrowserProfiles(): ServerProfile[] {
  try {
    const raw = window.localStorage.getItem(PROFILES_STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as ServerProfile[];
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

function writeBrowserProfiles(profiles: ServerProfile[]): void {
  try {
    window.localStorage.setItem(PROFILES_STORAGE_KEY, JSON.stringify(profiles));
  } catch {
    // ignore storage quota errors
  }
}

export async function listServerProfiles(): Promise<ServerProfile[]> {
  const tauri = getTauriInternals();
  if (tauri) {
    return await tauri.invoke<ServerProfile[]>("list_server_profiles");
  }
  return readBrowserProfiles();
}

export async function saveServerProfile(
  profile: Omit<ServerProfile, "lastUsedAt">
): Promise<ServerProfile[]> {
  const serverUrl = normalizeServerUrl(profile.serverUrl);
  const entry: ServerProfile = {
    ...profile,
    serverUrl,
    lastUsedAt: Date.now(),
  };

  const tauri = getTauriInternals();
  if (tauri) {
    return await tauri.invoke<ServerProfile[]>("save_server_profile", {
      profile: entry,
    });
  }

  const existing = readBrowserProfiles().filter(
    (p) => p.id !== entry.id && p.serverUrl !== entry.serverUrl
  );
  const next = [entry, ...existing].slice(0, 12);
  writeBrowserProfiles(next);
  return next;
}

export async function deleteServerProfile(
  profileId: string
): Promise<ServerProfile[]> {
  const tauri = getTauriInternals();
  if (tauri) {
    await tauri.invoke("delete_profile_token", { profileId });
    return await tauri.invoke<ServerProfile[]>("delete_server_profile", {
      profileId,
    });
  }
  const next = readBrowserProfiles().filter((p) => p.id !== profileId);
  writeBrowserProfiles(next);
  return next;
}

/** Saves token into the OS Keychain when running inside Tauri v2 desktop shell. */
export async function saveProfileTokenToKeychain(
  profileId: string,
  token: string
): Promise<boolean> {
  const tauri = getTauriInternals();
  if (!tauri || !token.trim()) return false;
  try {
    await tauri.invoke("save_profile_token", { profileId, token: token.trim() });
    return true;
  } catch {
    return false;
  }
}

/** Loads token from the OS Keychain when running inside Tauri v2 desktop shell. */
export async function loadProfileTokenFromKeychain(
  profileId: string
): Promise<string | null> {
  const tauri = getTauriInternals();
  if (!tauri) return null;
  try {
    return await tauri.invoke<string | null>("load_profile_token", { profileId });
  } catch {
    return null;
  }
}

/** Sends a native desktop notification when running inside Tauri v2. */
export async function sendNativeDesktopNotification(
  title: string,
  body: string
): Promise<boolean> {
  const tauri = getTauriInternals();
  if (!tauri) return false;
  try {
    await tauri.invoke("send_native_notification", { title, body });
    return true;
  } catch {
    return false;
  }
}

/** Updates the desktop system tray tooltip/badge with pending approval count. */
export async function updateTrayBadge(pendingApprovals: number): Promise<void> {
  const tauri = getTauriInternals();
  if (!tauri) return;
  try {
    await tauri.invoke("update_tray_badge", { pendingApprovals });
  } catch {
    // ignore
  }
}
