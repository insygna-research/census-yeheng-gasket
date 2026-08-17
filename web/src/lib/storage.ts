/**
 * Centralized key-value persistence.
 *
 * All persisted preference keys live here — do not scatter raw storage
 * calls across components/composables.
 *
 * Desktop (Tauri): NO localStorage. The Rust backend's
 * `~/.conga/app_config.json` is the only durable store — it is loaded
 * into an in-memory map before the app mounts (`initStorage`) and flushed
 * back (debounced, atomic tmp+rename) on every write. Session records are
 * NOT part of this map at all: the backend's session store
 * (`~/.conga/sessions`) owns them end to end.
 *
 * Browser (no embedded backend): the memory map hydrates from localStorage
 * once and writes through to it, preserving the pre-desktop behavior.
 */
import { invoke } from '@tauri-apps/api/core';
import { isTauri } from '@/lib/platform';

export const storageKeys = {
  theme: 'conga_theme_v2',
  sidebarWidth: 'conga_sidebar_width',
  sidebarCollapsed: 'conga_sidebar_collapsed',
  proxy: 'conga_proxy',
  gatewayToken: 'conga_gateway_token',
} as const;

const memory = new Map<string, string>();

let syncTimer: number | undefined;

/** Values that parse as JSON are stored parsed (readable file); raw strings
 * (sidebarWidth "260", sidebarCollapsed "false") are kept verbatim. */
function scheduleBackendSync(): void {
  if (!isTauri) return;
  if (syncTimer !== undefined) clearTimeout(syncTimer);
  syncTimer = window.setTimeout(() => {
    syncTimer = undefined;
    const config: Record<string, unknown> = {};
    for (const [key, raw] of memory) {
      try {
        config[key] = JSON.parse(raw);
      } catch {
        config[key] = raw;
      }
    }
    invoke('set_app_config', { config }).catch(e =>
      console.warn('app config sync failed:', e)
    );
  }, 500);
}

/**
 * Hydrate the memory map before the app mounts.
 * Tauri: from the backend's app_config.json. Browser: one-time copy of all
 * localStorage entries (including legacy keys, read-only). Never rejects.
 */
export async function initStorage(): Promise<void> {
  if (isTauri) {
    try {
      const config = await invoke<Record<string, unknown> | null>('get_app_config');
      if (config) {
        for (const [key, value] of Object.entries(config)) {
          if (value === undefined || value === null) continue;
          memory.set(key, typeof value === 'string' ? value : JSON.stringify(value));
        }
      }
    } catch (e) {
      console.warn('app config hydrate failed:', e);
    }
    return;
  }
  for (let i = 0; i < localStorage.length; i++) {
    const key = localStorage.key(i);
    if (key === null) continue;
    const raw = localStorage.getItem(key);
    if (raw !== null) memory.set(key, raw);
  }
}

export function readJSON<T>(key: string, fallback: T): T {
  const raw = memory.get(key);
  if (!raw) return fallback;
  try {
    return JSON.parse(raw) as T;
  } catch {
    return fallback;
  }
}

export function writeJSON(key: string, value: unknown): void {
  const raw = JSON.stringify(value);
  memory.set(key, raw);
  if (isTauri) scheduleBackendSync();
  else localStorage.setItem(key, raw);
}

export function readString(key: string, fallback = ''): string {
  return memory.get(key) ?? fallback;
}

export function writeString(key: string, value: string): void {
  memory.set(key, value);
  if (isTauri) scheduleBackendSync();
  else localStorage.setItem(key, value);
}
