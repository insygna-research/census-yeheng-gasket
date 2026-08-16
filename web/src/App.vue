<script setup lang="ts">
import { useSidebar } from './composables/useSidebar';
import { onMounted, onUnmounted, ref } from 'vue';
import AppSidebar from './components/AppSidebar.vue';
import ChatArea from './components/ChatArea.vue';
import { useChatStore } from './stores/chatStore';

const chatStore = useChatStore();
const { toggleSidebar } = useSidebar();

const sidebarRef = ref<InstanceType<typeof AppSidebar> | null>(null);

onMounted(() => {
  chatStore.syncFromBackend();
});

// ── Global shortcuts ────────────────────────────────────────

const onGlobalKeydown = (event: KeyboardEvent) => {
  const mod = event.metaKey || event.ctrlKey;
  if (!mod) return;
  switch (event.key.toLowerCase()) {
    case 'n':
      event.preventDefault();
      chatStore.createChat();
      break;
    case 'b':
      event.preventDefault();
      toggleSidebar();
      break;
    case 'k':
      event.preventDefault();
      sidebarRef.value?.focusSearch();
      break;
  }
};

onMounted(() => window.addEventListener('keydown', onGlobalKeydown));
onUnmounted(() => window.removeEventListener('keydown', onGlobalKeydown));
</script>

<template>
  <div class="flex h-screen w-full th-app-bg overflow-hidden">
    <AppSidebar ref="sidebarRef" />

    <!-- Main Chat Area -->
    <main class="flex-1 flex flex-col min-w-0 relative th-main-bg">
      <ChatArea
        v-if="chatStore.activeChatId"
        :chat-id="chatStore.activeChatId"
      />
    </main>
  </div>
</template>
