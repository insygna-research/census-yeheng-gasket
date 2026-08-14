<script setup lang="ts">
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { useSidebar } from '@/composables/useSidebar';
import { isMacOverlay } from '@/lib/platform';
import { useChatStore } from '@/stores/chatStore';
import type { Chat } from '@/types';
import {
  Check,
  MessageSquare,
  PanelLeftClose,
  PanelLeftOpen,
  Pencil,
  Plus,
  Search,
  X,
} from 'lucide-vue-next';
import { computed, nextTick, ref } from 'vue';

const chatStore = useChatStore();
const { sidebarWidth, isCollapsed, isResizing, onResizeStart, toggleSidebar } = useSidebar();

// ── Search + time grouping ──────────────────────────────────

const searchQuery = ref('');
const searchInputRef = ref<HTMLInputElement | null>(null);

const DAY_MS = 24 * 60 * 60 * 1000;

const groupedChats = computed(() => {
  const q = searchQuery.value.trim().toLowerCase();
  const sorted = [...chatStore.chats].sort((a, b) => b.updatedAt - a.updatedAt);
  const list = q ? sorted.filter(c => c.name.toLowerCase().includes(q)) : sorted;

  const startOfToday = new Date().setHours(0, 0, 0, 0);
  const groups: { label: string; chats: Chat[] }[] = [
    { label: 'Today', chats: [] },
    { label: 'Yesterday', chats: [] },
    { label: 'Earlier', chats: [] },
  ];
  for (const chat of list) {
    if (chat.updatedAt >= startOfToday) groups[0].chats.push(chat);
    else if (chat.updatedAt >= startOfToday - DAY_MS) groups[1].chats.push(chat);
    else groups[2].chats.push(chat);
  }
  return groups.filter(g => g.chats.length > 0);
});

const focusSearch = () => {
  if (isCollapsed.value) toggleSidebar();
  nextTick(() => searchInputRef.value?.focus());
};
defineExpose({ focusSearch });

// ── Rename ──────────────────────────────────────────────────

const editingChatId = ref<string | null>(null);
const editingName = ref('');

const startRename = (chatId: string, currentName: string, event: Event) => {
  event.stopPropagation();
  editingChatId.value = chatId;
  editingName.value = currentName;
};

const confirmRename = (chatId: string) => {
  if (editingName.value.trim()) {
    chatStore.renameChat(chatId, editingName.value.trim());
  }
  editingChatId.value = null;
};

const handleRenameKeydown = (event: KeyboardEvent, chatId: string) => {
  if (event.key === 'Enter') confirmRename(chatId);
  else if (event.key === 'Escape') editingChatId.value = null;
};

// ── Delete (two-click confirm) ──────────────────────────────

const confirmingDeleteId = ref<string | null>(null);
let confirmTimer: ReturnType<typeof setTimeout> | null = null;

const onDeleteClick = (chatId: string, event: Event) => {
  event.stopPropagation();
  if (confirmingDeleteId.value === chatId) {
    if (confirmTimer) clearTimeout(confirmTimer);
    confirmingDeleteId.value = null;
    chatStore.deleteChat(chatId);
    return;
  }
  confirmingDeleteId.value = chatId;
  if (confirmTimer) clearTimeout(confirmTimer);
  confirmTimer = setTimeout(() => (confirmingDeleteId.value = null), 2500);
};
</script>

<template>
  <aside
    class="relative flex flex-col th-sidebar-bg border-r th-border shrink-0 transition-all duration-300 ease-in-out"
    :class="isCollapsed ? 'items-center overflow-hidden' : ''"
    :style="{ width: (isCollapsed ? 48 : sidebarWidth) + 'px' }"
  >
    <!-- Collapsed rail -->
    <template v-if="isCollapsed">
      <div class="flex flex-col items-center gap-3 py-3 h-full" :class="{ 'pt-9': isMacOverlay }">
        <button
          class="w-8 h-8 rounded-lg bg-primary flex items-center justify-center text-primary-foreground hover:opacity-90 transition-opacity"
          @click="chatStore.createChat()"
          title="New Chat (⌘N)"
        >
          <Plus class="w-4 h-4" />
        </button>

        <div class="flex flex-col items-center gap-1.5 overflow-y-auto flex-1 min-h-0">
          <button
            v-for="chat in chatStore.chats"
            :key="chat.id"
            class="w-8 h-8 rounded-lg flex items-center justify-center text-xs font-bold transition-all"
            :class="chat.id === chatStore.activeChatId
              ? 'bg-primary/10 text-primary'
              : 'text-muted-foreground hover:bg-accent'"
            @click="chatStore.setActiveChat(chat.id)"
            :title="chat.name"
          >
            {{ chat.name.charAt(0).toUpperCase() }}
          </button>
        </div>

        <button
          class="w-8 h-8 rounded-lg flex items-center justify-center text-muted-foreground hover:bg-accent hover:text-foreground transition-colors"
          @click="toggleSidebar"
          title="Expand sidebar (⌘B)"
        >
          <PanelLeftOpen class="w-4 h-4" />
        </button>
      </div>
    </template>

    <!-- Expanded view -->
    <template v-else>
      <div
        class="p-4 flex justify-between items-center border-b th-border"
        :class="{ 'pl-[76px]': isMacOverlay }"
        data-tauri-drag-region
      >
        <h1 class="text-lg font-semibold flex items-center gap-2.5 th-text">
          <div class="w-7 h-7 rounded-lg bg-primary flex items-center justify-center">
            <MessageSquare class="w-4 h-4 text-primary-foreground" />
          </div>
          Chats
        </h1>
        <Button variant="ghost" size="icon" class="text-muted-foreground hover:text-foreground" @click="toggleSidebar" title="Collapse sidebar (⌘B)">
          <PanelLeftClose class="w-4 h-4" />
        </Button>
      </div>

      <!-- Search -->
      <div class="px-3 pt-3">
        <div class="flex items-center gap-2 px-2.5 rounded-lg th-surface th-border border">
          <Search class="w-3.5 h-3.5 th-text-dim shrink-0" />
          <input
            ref="searchInputRef"
            v-model="searchQuery"
            placeholder="Search chats... (⌘K)"
            class="flex-1 bg-transparent text-sm th-text py-1.5 outline-none placeholder:th-text-dim min-w-0"
          />
        </div>
      </div>

      <ScrollArea class="flex-1">
        <div class="flex flex-col gap-0.5 p-2">
          <template v-for="group in groupedChats" :key="group.label">
            <div class="px-3 pt-3 pb-1 text-[11px] font-semibold th-text-dim uppercase tracking-wider first:pt-1">
              {{ group.label }}
            </div>
            <div
              v-for="chat in group.chats"
              :key="chat.id"
              class="group flex items-center gap-2 px-3 py-2 rounded-xl cursor-pointer th-text-muted transition-colors duration-150 th-hover hover:th-text relative"
              :class="{ 'th-active-bg th-text': chat.id === chatStore.activeChatId }"
              @click="chatStore.setActiveChat(chat.id)"
            >
              <div class="flex-1 min-w-0">
                <input
                  v-if="editingChatId === chat.id"
                  v-model="editingName"
                  @click.stop
                  @keydown="(e) => handleRenameKeydown(e, chat.id)"
                  @blur="confirmRename(chat.id)"
                  class="w-full text-sm bg-background border border-primary/50 rounded px-1.5 py-0.5 text-foreground outline-none focus:ring-1 focus:ring-primary/30"
                  autofocus
                />
                <span v-else class="block text-sm font-medium th-text-secondary truncate">{{ chat.name }}</span>
              </div>

              <div class="opacity-0 group-hover:opacity-100 transition-opacity flex items-center gap-0.5 shrink-0">
                <button
                  @click="startRename(chat.id, chat.name, $event)"
                  class="p-1 rounded th-hover th-text-dim hover:th-text-secondary"
                  title="Rename"
                >
                  <Pencil class="w-3 h-3" />
                </button>
                <button
                  @click="onDeleteClick(chat.id, $event)"
                  class="p-1 rounded th-hover"
                  :class="confirmingDeleteId === chat.id ? 'text-destructive' : 'th-text-dim hover:text-destructive'"
                  :title="confirmingDeleteId === chat.id ? 'Click again to confirm' : 'Delete'"
                >
                  <Check v-if="confirmingDeleteId === chat.id" class="w-3 h-3" />
                  <X v-else class="w-3 h-3" />
                </button>
              </div>
            </div>
          </template>
          <div v-if="groupedChats.length === 0" class="px-3 py-6 text-center text-xs th-text-dim">
            No chats found
          </div>
        </div>
      </ScrollArea>

      <div class="p-3 border-t th-border">
        <Button variant="outline" class="w-full justify-start gap-2 th-surface-raised th-border th-hover th-text" @click="chatStore.createChat()">
          <Plus class="w-4 h-4" />
          New Chat
        </Button>
      </div>
    </template>

    <!-- Resize handle -->
    <div
      v-if="!isCollapsed"
      class="absolute top-0 right-0 bottom-0 w-1 cursor-col-resize z-20 hover:bg-primary/30 transition-colors"
      :class="isResizing ? 'bg-primary/40' : 'bg-transparent'"
      @mousedown="onResizeStart"
    />
  </aside>
</template>
