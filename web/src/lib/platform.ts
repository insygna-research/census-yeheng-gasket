/** True when running inside the Tauri desktop shell (vs a plain browser). */
export const isTauri =
  typeof window !== 'undefined' && !!(window as any).__TAURI_INTERNALS__;

/**
 * True when running inside the Tauri desktop shell on macOS, where the
 * overlay title bar leaves the traffic-light buttons floating over the UI
 * and the top-left corner needs clearance.
 */
export const isMacOverlay = isTauri && /Mac/.test(navigator.userAgent);
