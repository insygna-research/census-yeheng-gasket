<script setup lang="ts">
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { useResizableSidebar } from './composables/useResizableSidebar';
import { MessageSquare, Pencil, Plus, X, PanelLeftClose, PanelLeftOpen } from 'lucide-vue-next';
import { onMounted, onUnmounted, ref, watch } from 'vue';
import ChatArea from './components/ChatArea.vue';
import { useChatStore } from './stores/chatStore';

const chatStore = useChatStore();

const isCollapsed = ref(false);
const { sidebarWidth, isResizing, onResizeStart } = useResizableSidebar(isCollapsed);

onMounted(() => {
  chatStore.syncFromBackend();
});
onUnmounted(() => {
  if (saveTimer) clearTimeout(saveTimer);
});

// Persist to localStorage
let saveTimer: ReturnType<typeof setTimeout> | null = null;
const debouncedSave = () => {
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    localStorage.setItem('gasket_chats', JSON.stringify(chatStore.chats));
  }, 1000);
};
watch(() => chatStore.chats, debouncedSave, { deep: true });

const createNewChat = () => {
  chatStore.createChat();
};

const selectChat = (id: string) => {
  chatStore.setActiveChat(id);
};

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

const cancelRename = () => {
  editingChatId.value = null;
};

const handleRenameKeydown = (event: KeyboardEvent, chatId: string) => {
  if (event.key === 'Enter') {
    confirmRename(chatId);
  } else if (event.key === 'Escape') {
    cancelRename();
  }
};

const toggleSidebar = () => {
  isCollapsed.value = !isCollapsed.value;
  localStorage.setItem('gasket_sidebar_collapsed', String(isCollapsed.value));
};

// Session rename state
const editingChatId = ref<string | null>(null);
const editingName = ref('');

const formatDate = (ts?: number) => {
  if (!ts) return '';
  return new Date(ts).toLocaleDateString();
};

const truncateTitle = (name: string) => {
  return name.slice(0, 9) + '...';
};
</script>

<template>
  <div class="flex h-screen w-full th-app-bg overflow-hidden">
    <!-- Sidebar Drawer -->
    <aside
      class="relative flex flex-col th-sidebar-bg border-r th-border shrink-0 transition-all duration-300 ease-in-out"
      :class="isCollapsed ? 'items-center overflow-hidden' : ''"
      :style="{ width: (isCollapsed ? 48 : sidebarWidth) + 'px' }"
    >
      <!-- Collapsed view -->
      <template v-if="isCollapsed">
        <div class="flex flex-col items-center gap-3 py-3 h-full">
          <button
            class="w-8 h-8 rounded-lg bg-primary flex items-center justify-center text-primary-foreground hover:opacity-90 transition-opacity"
            @click="createNewChat"
            title="New Chat"
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
              @click="selectChat(chat.id)"
              :title="chat.name"
            >
              {{ chat.name.charAt(0).toUpperCase() }}
            </button>
          </div>

          <button
            class="w-8 h-8 rounded-lg flex items-center justify-center text-muted-foreground hover:bg-accent hover:text-foreground transition-colors"
            @click="toggleSidebar"
            title="Expand sidebar"
          >
            <PanelLeftOpen class="w-4 h-4" />
          </button>
        </div>
      </template>

      <!-- Expanded view -->
      <template v-else>
        <div class="p-4 flex justify-between items-center border-b th-border">
          <h1 class="text-lg font-semibold flex items-center gap-2.5 th-text">
            <div class="w-7 h-7 rounded-lg bg-primary flex items-center justify-center">
              <MessageSquare class="w-4 h-4 text-primary-foreground" />
            </div>
            Chats
          </h1>
          <Button variant="ghost" size="icon" class="text-muted-foreground hover:text-foreground" @click="toggleSidebar">
            <PanelLeftClose class="w-4 h-4" />
          </Button>
        </div>

        <ScrollArea class="flex-1">
          <div class="flex flex-col gap-0.5 p-2">
            <div
              v-for="chat in chatStore.chats"
              :key="chat.id"
              class="group flex items-center gap-3 px-3 py-2 rounded-xl cursor-pointer th-text-muted transition-all duration-200 th-hover hover:th-text relative"
              :class="{ 'th-active-bg th-text': chat.id === chatStore.activeChatId }"
              @click="selectChat(chat.id)"
            >
              <div class="flex-1 min-w-0">
                <div class="flex items-center justify-between">
                  <template v-if="editingChatId === chat.id">
                    <input
                      v-model="editingName"
                      @click.stop
                      @keydown="(e) => handleRenameKeydown(e, chat.id)"
                      @blur="confirmRename(chat.id)"
                      class="flex-1 text-sm bg-background border border-primary/50 rounded px-1.5 py-0.5 text-foreground outline-none focus:ring-1 focus:ring-primary/30 min-w-0"
                      autofocus
                    />
                  </template>
                  <template v-else>
                    <span class="text-sm font-medium th-text-secondary">{{ truncateTitle(chat.name) }}</span>
                  </template>
                </div>
                <span class="text-[11px] th-text-dim">
                  {{ formatDate(chat.messages[chat.messages.length - 1]?.timestamp) }}
                </span>
              </div>

              <div class="opacity-0 group-hover:opacity-100 transition-opacity flex items-center gap-0.5">
                <button
                  @click.stop="startRename(chat.id, chat.name, $event)"
                  class="p-1 rounded th-hover th-text-dim hover:th-text-secondary"
                  title="Rename"
                >
                  <Pencil class="w-3 h-3" />
                </button>
                <button
                  @click.stop="chatStore.deleteChat(chat.id)"
                  class="p-1 rounded th-hover th-text-dim hover:text-destructive"
                  title="Delete"
                >
                  <X class="w-3 h-3" />
                </button>
              </div>
            </div>
          </div>
        </ScrollArea>

        <div class="p-3 border-t th-border">
          <Button variant="outline" class="w-full justify-start gap-2 th-surface-raised th-border th-hover th-text" @click="createNewChat">
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

    <!-- Main Chat Area -->
    <main class="flex-1 flex flex-col min-w-0 relative th-main-bg">
      <ChatArea
        v-if="chatStore.activeChatId"
        :chat-id="chatStore.activeChatId"
      />
    </main>
  </div>
</template>
