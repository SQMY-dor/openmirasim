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
    try {
      return await tauri.invoke<ServerProfile[]>("list_server_profiles");
    } catch {
      // fallback to browser storage if desktop command unavailable
    }
  }
  return readBrowserProfiles();
}

export async function saveServerProfile(
  profile: Omit<ServerProfile, "lastUsedAt">
): Promise<ServerProfile[]> {
  const entry: ServerProfile = {
    ...profile,
    serverUrl: profile.serverUrl.trim().replace(/\/+$/, ""),
    lastUsedAt: Date.now(),
  };

  const tauri = getTauriInternals();
  if (tauri) {
    try {
      return await tauri.invoke<ServerProfile[]>("save_server_profile", {
        profile: entry,
      });
    } catch {
      // fallback
    }
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
    try {
      await tauri.invoke("delete_profile_token", { profileId }).catch(() => {});
      return await tauri.invoke<ServerProfile[]>("delete_server_profile", {
        profileId,
      });
    } catch {
      // fallback
    }
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
