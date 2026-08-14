/**
 * Centralized localStorage access.
 * All persisted preference keys live here — do not scatter raw
 * localStorage calls across components/composables.
 */
export const storageKeys = {
  chats: 'gasket_chats',
  theme: 'gasket_theme_v2',
  sidebarWidth: 'gasket_sidebar_width',
  sidebarCollapsed: 'gasket_sidebar_collapsed',
} as const;

export function readJSON<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : fallback;
  } catch {
    return fallback;
  }
}

export function writeJSON(key: string, value: unknown): void {
  localStorage.setItem(key, JSON.stringify(value));
}

export function readString(key: string, fallback = ''): string {
  return localStorage.getItem(key) ?? fallback;
}

export function writeString(key: string, value: string): void {
  localStorage.setItem(key, value);
}
