import { onUnmounted, ref } from 'vue';
import { readString, storageKeys, writeString } from '@/lib/storage';

const MIN_WIDTH = 200;
const MAX_WIDTH = 480;
const DEFAULT_WIDTH = 280;

// Module-level shared state — one sidebar per app.
const sidebarWidth = ref(DEFAULT_WIDTH);
const isCollapsed = ref(false);
const isResizing = ref(false);

let startX = 0;
let startWidth = 0;
let initialized = false;

const onResizeMove = (e: MouseEvent) => {
  if (!isResizing.value) return;
  const delta = e.clientX - startX;
  sidebarWidth.value = Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, startWidth + delta));
};

const onResizeEnd = () => {
  if (!isResizing.value) return;
  isResizing.value = false;
  document.body.style.cursor = '';
  document.body.style.userSelect = '';
  writeString(storageKeys.sidebarWidth, String(sidebarWidth.value));
};

function init() {
  if (initialized) return;
  initialized = true;

  const savedWidth = parseInt(readString(storageKeys.sidebarWidth), 10);
  if (!Number.isNaN(savedWidth)) {
    sidebarWidth.value = Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, savedWidth));
  }
  isCollapsed.value = readString(storageKeys.sidebarCollapsed) === 'true';

  window.addEventListener('mousemove', onResizeMove);
  window.addEventListener('mouseup', onResizeEnd);
}

export function useSidebar() {
  init();

  onUnmounted(() => {
    // Listeners live for the app lifetime; only reset transient drag state.
    isResizing.value = false;
  });

  const onResizeStart = (e: MouseEvent) => {
    if (isCollapsed.value) return;
    isResizing.value = true;
    startX = e.clientX;
    startWidth = sidebarWidth.value;
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
  };

  const toggleSidebar = () => {
    isCollapsed.value = !isCollapsed.value;
    writeString(storageKeys.sidebarCollapsed, String(isCollapsed.value));
  };

  return {
    sidebarWidth,
    isCollapsed,
    isResizing,
    onResizeStart,
    toggleSidebar,
  };
}
