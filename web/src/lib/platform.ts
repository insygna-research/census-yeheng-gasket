/**
 * True when running inside the Tauri desktop shell on macOS, where the
 * overlay title bar leaves the traffic-light buttons floating over the UI
 * and the top-left corner needs clearance.
 */
export const isMacOverlay =
  typeof window !== 'undefined' &&
  !!(window as any).__TAURI_INTERNALS__ &&
  /Mac/.test(navigator.userAgent);
