<script setup lang="ts">
import { AlertCircle, Check, CheckCheck } from 'lucide-vue-next';
import { useTheme } from '../composables/useTheme';
import { renderMessageContent } from '../lib/markdown';
import { computed, nextTick, ref, watch } from 'vue';
import type { Message } from '../types';
import MessageThoughtsPanel from './MessageThoughtsPanel.vue';

const props = defineProps<{
  message: Message;
  isLastBotMessage: boolean;
  isThinking: boolean;
  isReceiving: boolean;
  subagentPhase: 'idle' | 'running' | 'synthesizing' | 'completed';
}>();

const emit = defineEmits<{
  (e: 'retry'): void;
}>();

const { markdownStyle } = useTheme();

// Mermaid rendering — lazy loaded to avoid init-time crashes
const mermaidContainerRef = ref<HTMLDivElement | null>(null);
let mermaidModule: typeof import('mermaid') | null = null;

const renderMermaid = async () => {
  await nextTick();
  if (!mermaidContainerRef.value) return;
  const elements = mermaidContainerRef.value.querySelectorAll('.mermaid-diagram');
  if (elements.length === 0) return;

  // Lazy-load mermaid only when needed
  if (!mermaidModule) {
    try {
      mermaidModule = await import('mermaid');
      mermaidModule.default.initialize({ startOnLoad: false, securityLevel: 'strict' });
    } catch (e) {
      console.error('Failed to load mermaid:', e);
      return;
    }
  }

  for (const el of elements) {
    // Skip already rendered diagrams
    if ((el as HTMLElement).dataset.rendered === 'true') continue;
    try {
      const source = decodeURIComponent((el as HTMLElement).dataset.source || '');
      if (!source) continue;
      const { svg } = await mermaidModule.default.render(
        `mermaid-${Math.random().toString(36).substr(2, 9)}`,
        source
      );
      (el as HTMLElement).innerHTML = svg;
      (el as HTMLElement).dataset.rendered = 'true';
    } catch (e) {
      console.error('Mermaid render error:', e);
      (el as HTMLElement).dataset.rendered = 'true';
    }
  }
};

const parsedContent = computed(() => renderMessageContent(props.message.content, props.isReceiving));

// Render mermaid when the component is mounted/updated and not actively streaming.
// We skip rendering during streaming to avoid flooding the renderer with
// partial diagram sources that may be syntactically invalid.
const shouldRenderMermaid = computed(() => !props.isReceiving);

watch(shouldRenderMermaid, (canRender) => {
  if (canRender) renderMermaid();
}, { immediate: true });

const copyCode = (event: MouseEvent) => {
  const target = event.target as HTMLElement;
  const btn = target.closest('.copy-btn') as HTMLElement | null;
  if (!btn) return;
  const pre = btn.closest('.relative')?.querySelector('pre');
  if (pre) {
    const code = pre.textContent || '';
    navigator.clipboard.writeText(code).then(() => {
      const originalText = btn.textContent;
      btn.textContent = 'Copied!';
      setTimeout(() => (btn.textContent = originalText), 1500);
    });
  }
};

const formatTime = (ts: number) => {
  return new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
};

const isStreaming = computed(() => props.isLastBotMessage && props.isReceiving);
</script>

<template>
  <div class="py-1"
    :class="message.role === 'user' ? 'flex justify-end' : message.role === 'system' ? 'flex justify-center' : 'flex justify-start'">
    
    <!-- System message -->
    <div v-if="message.role === 'system'" class="text-[11px] th-text-muted px-3 py-1 th-active-bg rounded-full">
      {{ message.content }}
    </div>

    <!-- User message -->
    <div v-else-if="message.role === 'user'" class="flex flex-col items-end gap-0.5 max-w-[85%] md:max-w-[75%]">
      <div class="px-4 py-2.5 rounded-2xl bg-muted th-text text-[15px]">
        <div class="whitespace-pre-wrap">{{ message.content }}</div>
      </div>
      <div class="flex items-center gap-1 px-1">
        <span class="text-[11px] th-text-dim">{{ formatTime(message.timestamp) }}</span>
        <Check v-if="message.status === 'sending'" class="w-3 h-3 th-text-dim" />
        <CheckCheck v-else-if="message.status === 'sent'" class="w-3 h-3 th-text-dim" />
        <div v-else-if="message.status === 'error'" class="flex items-center gap-1 text-destructive cursor-pointer hover:underline" @click="emit('retry')">
          <AlertCircle class="w-3 h-3" />
          <span class="text-[11px]">Failed</span>
        </div>
      </div>
    </div>

    <!-- Bot message: full-width document layout, no bubble/avatar -->
    <div v-else class="flex flex-col gap-1 w-full min-w-0">
      <MessageThoughtsPanel
        :message="message"
        :is-thinking="isThinking"
        :is-last-bot-message="isLastBotMessage"
        :subagents="message.subagents"
        :subagent-phase="subagentPhase"
      />

      <!-- Content -->
      <div v-if="message.content || isStreaming" class="th-text text-[15px] min-w-0 w-full">
        <div ref="mermaidContainerRef" class="prose max-w-none" :data-md-style="markdownStyle" v-html="parsedContent" @click="copyCode" />
        <!-- Streaming cursor -->
        <span v-if="isStreaming" class="inline-block w-2 h-4 bg-primary/80 ml-0.5 align-middle animate-pulse rounded-sm" />
      </div>
    </div>
  </div>
</template>

<style>
.prose pre { margin: 0; }
.prose p { margin-bottom: 0.5em; }
.prose p:last-child { margin-bottom: 0; }
</style>
