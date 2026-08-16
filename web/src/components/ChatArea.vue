<script setup lang="ts">
import { ScrollArea } from '@/components/ui/scroll-area';
import { AlertCircle, ArrowDown, FileText, Lightbulb, Rocket, SearchCode, Sparkles, X as XIcon } from 'lucide-vue-next';
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue';
import { useChatSession } from '../composables/useChatSession';
import { useChatStore } from '../stores/chatStore';
import ApprovalDialog from './ApprovalDialog.vue';
import ChatHeader from './ChatHeader.vue';
import ChatInput from './ChatInput.vue';
import ChatTimeDivider from './ChatTimeDivider.vue';
import MessageBubble from './MessageBubble.vue';

const props = defineProps<{ chatId: string }>();
const emit = defineEmits<{ (e: 'connection-status', status: boolean): void }>();

const chatStore = useChatStore();
const session = useChatSession(computed(() => props.chatId));

watch(() => session.isConnected, (val) => {
  emit('connection-status', val);
}, { immediate: true });

// Scroll
const scrollAreaRef = ref<InstanceType<typeof ScrollArea> | null>(null);
const showScrollButton = ref(false);
const userScrolledUp = ref(false);

const getScrollElement = (scrollArea: InstanceType<typeof ScrollArea> | null): HTMLElement | null => {
  if (!scrollArea) return null;
  const el = scrollArea.$el as HTMLElement;
  if (!el) return null;
  const viewport = el.querySelector('[data-radix-scroll-area-viewport]') as HTMLElement;
  return viewport || el;
};

const scrollToBottom = async (force = false) => {
  await nextTick();
  const scrollEl = getScrollElement(scrollAreaRef.value);
  if (!scrollEl) return;
  if (!force && userScrolledUp.value) return;
  scrollEl.scrollTo({ top: scrollEl.scrollHeight, behavior: force ? 'smooth' : 'auto' });
};

const forceScrollToBottom = () => {
  userScrolledUp.value = false;
  scrollToBottom(true);
};

let scrollListener: (() => void) | null = null;

const setupScrollObserver = () => {
  nextTick(() => {
    const scrollEl = getScrollElement(scrollAreaRef.value);
    if (!scrollEl) return;
    const onScroll = () => {
      const distFromBottom = scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight;
      const nearBottom = distFromBottom < 100;
      showScrollButton.value = distFromBottom > 200;
      userScrolledUp.value = !nearBottom;
    };
    scrollEl.addEventListener('scroll', onScroll);
    scrollListener = () => scrollEl.removeEventListener('scroll', onScroll);
  });
};

onUnmounted(() => {
  scrollListener?.();
});

// Messages
const messages = computed(() => chatStore.activeMessages);
const hasUserMessages = computed(() => messages.value.some(m => m.role === 'user' || m.role === 'bot'));
const chatTitle = computed(() => chatStore.getChat(props.chatId)?.name || 'Chat');

// Esc stops an in-flight generation
const onKeydown = (event: KeyboardEvent) => {
  if (event.key === 'Escape' && (session.isReceiving || session.isThinking)) {
    session.stopGenerating();
  }
};

watch(() => messages.value.length, () => scrollToBottom());
watch(() => props.chatId, () => {
  session.fetchContext();
  userScrolledUp.value = false;
  nextTick(() => scrollToBottom(true));
});

onMounted(() => {
  session.connect();
  session.fetchContext();
  nextTick(() => scrollToBottom(true));
  setupScrollObserver();
  window.addEventListener('keydown', onKeydown);
});

onUnmounted(() => {
  window.removeEventListener('keydown', onKeydown);
});

const suggestedPrompts = [
  { icon: Lightbulb, text: 'Explain how this project is structured' },
  { icon: SearchCode, text: 'Help me find and fix bugs in my code' },
  { icon: FileText, text: 'Write a unit test for a function' },
  { icon: Rocket, text: 'Suggest performance improvements' },
];

const sendPrompt = (prompt: string) => {
  session.sendMessage(prompt);
};

const retryMessage = (msgId: string, content: string) => {
  session.retryMessage(msgId, content);
};

const clearHistory = () => {
  chatStore.clearChatMessages(props.chatId);
};

// Approval dialog
const currentApproval = computed(() => {
  const vals = session.pendingApprovals.values();
  const first = vals.next();
  return first.value || null;
});

const handleApprovalResponse = (requestId: string, approved: boolean, remember: boolean) => {
  session.sendApprovalResponse(requestId, approved, remember);
};
</script>

<template>
  <div class="flex h-full w-full relative">
    <div class="flex flex-col flex-1 min-w-0">
      <ChatHeader
        :chat-title="chatTitle"
        :is-connected="session.isConnected"
        :session-status="session.sessionStatus"
        :show-reconnect-button="session.showReconnectButton"
        :context-stats="session.contextStats"
        :usage-color="session.usageColor"
        :is-compacting="session.isCompacting"
        @reconnect="session.manualReconnect"
        @compact="session.forceCompact"
        @clear-history="clearHistory"
      />

      <!-- Error Banner -->
      <div v-if="session.errorBanner"
        class="mx-4 mt-2 flex items-center gap-2 bg-destructive/15 border border-destructive/30 text-destructive px-3 py-2 rounded-lg text-xs animate-in fade-in slide-in-from-top-2 duration-300 shrink-0">
        <AlertCircle class="w-4 h-4 shrink-0 text-destructive" />
        <span class="flex-1">{{ session.errorBanner }}</span>
        <button @click="session.dismissError" class="p-0.5 hover:bg-destructive/20 rounded transition-colors text-destructive">
          <XIcon class="w-3.5 h-3.5" />
        </button>
      </div>

      <!-- Messages -->
      <ScrollArea class="flex-1 p-4" ref="scrollAreaRef">
        <!-- Empty State -->
        <div v-if="!hasUserMessages"
          class="flex flex-col items-center justify-center h-full max-w-3xl mx-auto text-center py-16 th-text">
          <div class="w-14 h-14 rounded-2xl bg-primary flex items-center justify-center mb-5">
            <Sparkles class="w-7 h-7 text-primary-foreground" />
          </div>
          <h2 class="text-xl font-semibold th-text mb-2">How can I help you today?</h2>
          <p class="th-text-muted mb-6 text-sm">Ask me anything about your code, project, or ideas.</p>
          <div class="grid grid-cols-1 sm:grid-cols-2 gap-2 w-full">
            <button v-for="(prompt, idx) in suggestedPrompts" :key="idx" @click="sendPrompt(prompt.text)"
              :disabled="!session.isConnected"
              class="flex items-center gap-2.5 p-3 th-surface th-border th-hover rounded-xl text-left text-sm th-text-secondary hover:th-text transition-colors duration-150 disabled:opacity-40 disabled:cursor-not-allowed group">
              <component :is="prompt.icon" class="w-4 h-4 shrink-0 th-text-dim group-hover:th-text transition-colors" />
              <span>{{ prompt.text }}</span>
            </button>
          </div>
        </div>

        <!-- Messages List -->
        <div v-else class="flex flex-col gap-1 max-w-3xl mx-auto w-full pb-4 px-4">
          <template v-for="(msg, idx) in messages" :key="msg.id">
            <ChatTimeDivider
              v-if="idx > 0 && msg.timestamp - messages[idx - 1].timestamp > 5 * 60 * 1000"
              :timestamp="msg.timestamp"
            />
            <MessageBubble
              :message="msg"
              :is-last-bot-message="msg.role === 'bot' && idx === messages.length - 1"
              :is-thinking="session.isThinking"
              :is-receiving="session.isReceiving"
              :subagent-phase="session.subagentPhase"
              @retry="() => retryMessage(msg.id, msg.content)"
            />
          </template>

          <!-- Typing indicator -->
          <div v-if="session.isReceiving && !session.isThinking" class="flex mt-2">
            <div class="px-3 py-2 rounded-2xl th-typing-bg flex items-center gap-1">
              <span class="w-1.5 h-1.5 bg-muted-foreground rounded-full animate-bounce" style="animation-delay: 0ms" />
              <span class="w-1.5 h-1.5 bg-muted-foreground rounded-full animate-bounce" style="animation-delay: 150ms" />
              <span class="w-1.5 h-1.5 bg-muted-foreground rounded-full animate-bounce" style="animation-delay: 300ms" />
            </div>
          </div>

        </div>
      </ScrollArea>

      <!-- Scroll to bottom button -->
      <Transition enter-active-class="transition-all duration-200 ease-out" leave-active-class="transition-all duration-150 ease-in"
        enter-from-class="opacity-0 translate-y-2" leave-to-class="opacity-0 translate-y-2">
        <button v-if="showScrollButton" @click="forceScrollToBottom"
          class="absolute bottom-28 left-1/2 -translate-x-1/2 z-10 flex items-center gap-1.5 px-3 py-1.5 bg-popover hover:bg-accent border border-border rounded-full text-foreground text-xs shadow-lg backdrop-blur-sm transition-colors">
          <ArrowDown class="w-3.5 h-3.5" />
          New messages
        </button>
      </Transition>

      <ChatInput
        :is-connected="session.isConnected"
        :session-status="session.sessionStatus"
        :is-thinking="session.isThinking"
        :is-receiving="session.isReceiving"
        @send="session.sendMessage"
        @stop="session.stopGenerating"
      />

      <ApprovalDialog
        :request="currentApproval"
        @respond="handleApprovalResponse"
      />
    </div>
  </div>
</template>
