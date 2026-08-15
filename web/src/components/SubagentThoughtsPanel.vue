<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue';
import {
  Sparkles,
  ChevronRight,
  Loader2,
  CheckCircle,
  XCircle,
  Wrench,
  Terminal,
  ArrowRight,
  Users,
} from 'lucide-vue-next';
import { Collapsible, CollapsibleTrigger, CollapsibleContent } from '@/components/ui/collapsible';
import type { SubagentState, SubagentToolCall } from '../types';

const props = withDefaults(
  defineProps<{
    subagents: SubagentState[];
    phase?: 'idle' | 'running' | 'synthesizing' | 'completed';
  }>(),
  { phase: 'completed' }
);

const toolExpandedMap = ref<Record<string, boolean>>({});


// Reactive clock for running durations: formatDuration re-evaluates every
// second while any subagent is running, so the timer visibly ticks.
const now = ref(Date.now());
let tickTimer: ReturnType<typeof setInterval> | null = null;
const anyRunning = computed(() => props.subagents.some(s => s.status === 'running'));
watch(anyRunning, running => {
  if (running && tickTimer === null) {
    tickTimer = setInterval(() => { now.value = Date.now(); }, 1000);
  } else if (!running && tickTimer !== null) {
    clearInterval(tickTimer);
    tickTimer = null;
  }
}, { immediate: true });
onUnmounted(() => {
  if (tickTimer !== null) {
    clearInterval(tickTimer);
    tickTimer = null;
  }
});

const sortedSubagents = computed(() =>
  [...props.subagents].sort((a, b) => a.index - b.index)
);

const hasAnySubagents = computed(() => props.subagents.length > 0);

function isToolExpanded(toolId: string): boolean {
  if (toolId in toolExpandedMap.value) {
    return toolExpandedMap.value[toolId];
  }
  return false;
}

function toggleTool(toolId: string) {
  toolExpandedMap.value[toolId] = !toolExpandedMap.value[toolId];
}

function statusLabel(status: SubagentState['status']) {
  switch (status) {
    case 'running': return 'Running';
    case 'completed': return 'Done';
    case 'error': return 'Error';
  }
}

function statusClasses(status: SubagentState['status']) {
  switch (status) {
    case 'running': return 'bg-primary/10 text-primary border-primary/20';
    case 'completed': return 'bg-emerald-500/10 text-emerald-600 dark:text-emerald-400 border-emerald-500/20';
    case 'error': return 'bg-destructive/10 text-destructive border-destructive/20';
  }
}

function statusIconClasses(status: SubagentState['status']) {
  switch (status) {
    case 'running': return 'text-primary';
    case 'completed': return 'text-emerald-600 dark:text-emerald-400';
    case 'error': return 'text-destructive';
  }
}

function toolStatusClasses(status: SubagentToolCall['status']) {
  switch (status) {
    case 'running': return 'bg-primary/10 text-primary border-primary/20';
    case 'complete': return 'bg-emerald-500/10 text-emerald-600 dark:text-emerald-400 border-emerald-500/20';
    case 'error': return 'bg-destructive/10 text-destructive border-destructive/20';
  }
}

function iconForStatus(status: SubagentState['status']) {
  switch (status) {
    case 'running': return Loader2;
    case 'completed': return CheckCircle;
    case 'error': return XCircle;
  }
}

function toolIconForStatus(status: SubagentToolCall['status']) {
  switch (status) {
    case 'running': return Loader2;
    case 'complete': return CheckCircle;
    case 'error': return XCircle;
  }
}

function formatDuration(start: number, end?: number) {
  const ms = (end ?? now.value) - start; // reads `now` → reactive while running
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60000)}m ${Math.floor((ms % 60000) / 1000)}s`;
}
</script>
<template>
  <div v-if="hasAnySubagents" class="w-full my-1 relative">
    <!-- Tree of subagent nodes -->
    <div
      class="space-y-3 transition-opacity duration-300"
      :class="{ 'opacity-0 pointer-events-none': phase === 'synthesizing' }"
    >
      <div v-for="subagent in sortedSubagents" :key="subagent.id">
        <!-- Node header -->
        <div class="flex items-center justify-between gap-2 py-1">
          <div class="flex items-center gap-2 min-w-0">
            <component
              :is="iconForStatus(subagent.status)"
              class="w-3.5 h-3.5 shrink-0"
              :class="[
                statusIconClasses(subagent.status),
                { 'animate-spin': subagent.status === 'running' },
              ]"
            />
            <span class="font-medium text-xs truncate">
              {{ subagent.task }}
            </span>
            <span class="text-[11px] opacity-60 shrink-0">
              #{{ subagent.index }}
            </span>
          </div>
          <div class="flex items-center gap-1.5 shrink-0">
            <span
              class="text-[11px] px-1.5 py-0.5 rounded-full border"
              :class="statusClasses(subagent.status)"
            >
              {{ statusLabel(subagent.status) }}
            </span>
            <span v-if="subagent.toolCount > 0" class="text-[11px] opacity-70 flex items-center gap-0.5">
              <Wrench class="w-3 h-3" />
              {{ subagent.toolCount }}
            </span>
            <span class="text-[11px] opacity-70">
              {{ formatDuration(subagent.startTime, subagent.endTime) }}
            </span>
          </div>
        </div>

        <!-- Tree children: indented with a vertical connector line -->
        <div class="ml-[7px] border-l border-border/60 pl-3 space-y-2 text-xs">
          <!-- Thinking -->
          <div v-if="subagent.thinking" class="pt-1">
            <div class="flex items-center gap-1 text-[11px] opacity-70 uppercase tracking-wider mb-1">
              <Sparkles class="w-3 h-3" />
              <span>Thinking</span>
            </div>
            <div class="th-text-secondary whitespace-pre-wrap leading-relaxed text-[11px] break-words">
              {{ subagent.thinking }}
            </div>
          </div>

          <!-- Tool calls -->
          <div v-if="subagent.toolCalls.length > 0" class="space-y-1 pt-1">
            <div class="flex items-center gap-1 text-[11px] opacity-70 uppercase tracking-wider">
              <Wrench class="w-3 h-3" />
              <span>Tool Calls</span>
            </div>
            <Collapsible
              v-for="tool in subagent.toolCalls"
              :key="tool.id"
              :open="isToolExpanded(tool.id)"
              class="rounded-lg border overflow-hidden"
              :class="toolStatusClasses(tool.status)"
            >
              <CollapsibleTrigger as-child @click="toggleTool(tool.id)">
                <button class="w-full flex items-center gap-2 px-2 py-1.5 text-left">
                  <component
                    :is="toolIconForStatus(tool.status)"
                    class="w-3 h-3 shrink-0"
                    :class="{ 'animate-spin': tool.status === 'running' }"
                  />
                  <span class="font-medium truncate flex-1 text-[11px]">
                    {{ tool.name }}
                  </span>
                  <span
                    v-if="tool.duration"
                    class="text-[11px] opacity-70 shrink-0"
                  >
                    {{ tool.duration }}
                  </span>
                  <ChevronRight
                    class="w-3 h-3 shrink-0 opacity-60 transition-transform"
                    :class="{ 'rotate-90': isToolExpanded(tool.id) }"
                  />
                </button>
              </CollapsibleTrigger>
              <CollapsibleContent>
                <div class="px-2 pb-2 space-y-1.5">
                  <div v-if="tool.arguments" class="space-y-0.5">
                    <div class="flex items-center gap-1 text-[11px] opacity-70 uppercase tracking-wider">
                      <Terminal class="w-2.5 h-2.5" />
                      <span>Input</span>
                    </div>
                    <div class="font-mono text-[11px] bg-black/5 dark:bg-white/5 rounded p-1.5 whitespace-pre-wrap break-all max-h-32 overflow-auto">
                      {{ tool.arguments }}
                    </div>
                  </div>
                  <div v-if="tool.output" class="space-y-0.5">
                    <div class="flex items-center gap-1 text-[11px] opacity-70 uppercase tracking-wider">
                      <ArrowRight class="w-2.5 h-2.5" />
                      <span>Output</span>
                    </div>
                    <div class="font-mono text-[11px] bg-black/5 dark:bg-white/5 rounded p-1.5 whitespace-pre-wrap break-words max-h-40 overflow-auto">
                      {{ tool.output }}
                    </div>
                  </div>
                </div>
              </CollapsibleContent>
            </Collapsible>
          </div>

          <!-- Content / Response -->
          <div v-if="subagent.content" class="pt-1 pb-1">
            <div class="flex items-center gap-1 text-[11px] opacity-70 uppercase tracking-wider mb-1">
              <Users class="w-3 h-3" />
              <span>Response</span>
            </div>
            <div class="th-text-secondary whitespace-pre-wrap leading-relaxed text-[11px] break-words">
              {{ subagent.content }}
            </div>
          </div>

          <!-- Error -->
          <div v-if="subagent.error" class="pt-1 pb-1">
            <div class="flex items-center gap-1 text-[11px] text-destructive uppercase tracking-wider mb-1">
              <XCircle class="w-3 h-3" />
              <span>Error</span>
            </div>
            <div class="text-destructive whitespace-pre-wrap leading-relaxed text-[11px]">
              {{ subagent.error }}
            </div>
          </div>
        </div>
      </div>
    </div>

    <!-- Synthesizing overlay -->
    <Transition
      enter-active-class="transition-all duration-200 ease-out"
      leave-active-class="transition-all duration-150 ease-in"
      enter-from-class="opacity-0"
      leave-to-class="opacity-0"
    >
      <div
        v-if="phase === 'synthesizing'"
        class="flex items-center justify-center gap-2 py-6 text-sm th-text-muted"
      >
        <Loader2 class="w-4 h-4 animate-spin" />
        <span>Synthesizing results...</span>
      </div>
    </Transition>
  </div>
</template>
