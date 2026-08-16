<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import {
  Sparkles,
  ChevronDown,
  ChevronRight,
  Loader2,
  CheckCircle,
  XCircle,
  Wrench,
  Terminal,
  ArrowRight,
} from 'lucide-vue-next';
import { Collapsible, CollapsibleTrigger, CollapsibleContent } from '@/components/ui/collapsible';
import type { Message, SubagentState, TimelineItem, ToolCall } from '../types';
import SubagentThoughtsPanel from './SubagentThoughtsPanel.vue';

const props = defineProps<{
  message: Message;
  isThinking: boolean;
  isLastBotMessage: boolean;
  subagents?: SubagentState[];
  subagentPhase?: 'idle' | 'running' | 'synthesizing' | 'completed';
}>();

const expanded = ref(false);

const hasThinking = computed(() => !!props.message.thinking);
const hasTools = computed(() => (props.message.toolCalls?.length || 0) > 0);
const hasSubagents = computed(() => (props.subagents?.length || 0) > 0);

// Auto-expand when subagents are active so users see real-time progress
watch(
  () => props.subagents,
  (subagents) => {
    if (
      subagents &&
      subagents.length > 0 &&
      (props.subagentPhase === 'running' || props.subagentPhase === 'synthesizing')
    ) {
      expanded.value = true;
    }
  },
  { immediate: true, deep: true }
);
const runningToolCount = computed(() => props.message.toolCalls?.filter(t => t.status === 'running').length || 0);
const completedToolCount = computed(() => props.message.toolCalls?.filter(t => t.status === 'complete').length || 0);
const errorToolCount = computed(() => props.message.toolCalls?.filter(t => t.status === 'error').length || 0);
const totalToolCount = computed(() => props.message.toolCalls?.length || 0);
const isActive = computed(() => props.isLastBotMessage && (props.isThinking || runningToolCount.value > 0));

/** Build a chronological timeline of thinking chunks and tool calls. */
const timeline = computed((): TimelineItem[] => {
  const rawItems: TimelineItem[] = [];

  const chunks = props.message.thinkingChunks || [];
  for (const chunk of chunks) {
    rawItems.push({ type: 'thinking', content: chunk.content, timestamp: chunk.timestamp });
  }

  const tools = props.message.toolCalls || [];
  for (const tool of tools) {
    rawItems.push({ type: 'tool_call', tool, timestamp: tool.startTime || 0 });
  }

  rawItems.sort((a, b) => a.timestamp - b.timestamp);

  const items: TimelineItem[] = [];
  for (const item of rawItems) {
    if (item.type === 'thinking') {
      const last = items[items.length - 1];
      if (last && last.type === 'thinking') {
        last.content += item.content;
      } else {
        items.push({ ...item });
      }
    } else {
      items.push({ ...item });
    }
  }

  return items;
});

/** Per-tool-call expanded state map. */
const toolExpandedMap = ref<Record<string, boolean>>({});

// ── Subagent nesting ────────────────────────────────────────
// Subagents are launched by the `spawn_subagents` tool; each child agent is
// rendered indented beneath the tool call that started it (matched by
// startTime so sequential batches group under their own call).
const spawnTools = computed(() =>
  (props.message.toolCalls || []).filter(t => t.name === 'spawn_subagents')
);

function subagentsForTool(tool: ToolCall): SubagentState[] {
  if (!props.subagents) return [];
  const start = tool.startTime || 0;
  const laterStarts = spawnTools.value
    .map(t => t.startTime || 0)
    .filter(ts => ts > start);
  const end = laterStarts.length ? Math.min(...laterStarts) : Infinity;
  return props.subagents.filter(s => s.startTime >= start && s.startTime < end);
}

/** Subagents not attributable to any spawn_subagents call (e.g. restored
 * history without tool timestamps) still render at the bottom. */
const orphanSubagents = computed(() => {
  if (!props.subagents) return [];
  if (spawnTools.value.length === 0) return props.subagents;
  const firstStart = Math.min(...spawnTools.value.map(t => t.startTime || 0));
  return props.subagents.filter(s => s.startTime < firstStart);
});

function isToolExpanded(tool: ToolCall): boolean {
  // Default to expanded for running tools, collapsed for completed/error
  if (tool.id in toolExpandedMap.value) {
    return toolExpandedMap.value[tool.id];
  }
  return tool.status === 'running';
}

function toggleTool(toolId: string) {
  toolExpandedMap.value[toolId] = !toolExpandedMap.value[toolId];
}

function statusLabel(status: ToolCall['status']) {
  switch (status) {
    case 'running':
      return 'Running';
    case 'complete':
      return 'Success';
    case 'error':
      return 'Error';
  }
}

function statusClasses(status: ToolCall['status']) {
  switch (status) {
    case 'running':
      return 'bg-primary/10 text-primary border-primary/20';
    case 'complete':
      return 'bg-emerald-500/10 text-emerald-600 dark:text-emerald-400 border-emerald-500/20';
    case 'error':
      return 'bg-destructive/10 text-destructive border-destructive/20';
  }
}

function iconForStatus(status: ToolCall['status']) {
  switch (status) {
    case 'running':
      return Loader2;
    case 'complete':
      return CheckCircle;
    case 'error':
      return XCircle;
  }
}

/** Friendly display names for wire-level tool names. spawn_subagents shows
 * the number of child agents attributed to the call once they start. */
function toolDisplayName(tool: ToolCall): string {
  if (tool.name === 'spawn_subagents') {
    const count = subagentsForTool(tool).length;
    return count > 0 ? `Parallel Subagents ×${count}` : 'Parallel Subagents';
  }
  return tool.name;
}
</script>

<template>
  <div v-if="hasThinking || hasTools || hasSubagents" class="w-full my-1">
    <!-- Collapsible header -->
    <button
      @click="expanded = !expanded"
      class="w-full flex items-center justify-between gap-2 px-3 py-2 rounded-xl border transition-all"
      :class="[
        isActive
          ? 'bg-primary/5 border-primary/20'
          : 'th-surface-raised th-border th-hover'
      ]"
    >
      <div class="flex items-center gap-2">
        <span class="relative flex h-2 w-2">
          <span
            v-if="isActive"
            class="animate-ping absolute inline-flex h-full w-full rounded-full bg-primary opacity-75"
          />
          <span
            class="relative inline-flex rounded-full h-2 w-2"
            :class="isActive ? 'bg-primary' : 'bg-muted-foreground'"
          />
        </span>
        <Sparkles class="w-3.5 h-3.5 text-primary" />
        <span class="text-[11px] font-medium th-text-secondary">
          Thoughts
        </span>
        <span
          v-if="hasTools"
          class="text-[11px] th-text-muted flex items-center gap-1"
        >
          <Wrench class="w-3 h-3" />
          <span v-if="errorToolCount > 0" class="text-destructive">{{ errorToolCount }} failed</span>
          <span v-else>{{ completedToolCount }}/{{ totalToolCount }}</span>
        </span>
      </div>

      <div class="flex items-center gap-1.5 text-[11px] th-text-muted">
        <span v-if="!expanded" class="hidden sm:inline">Expand to view model thoughts</span>
        <ChevronDown
          class="w-4 h-4 text-muted-foreground transition-transform"
          :class="{ 'rotate-180': expanded }"
        />
      </div>
    </button>

    <!-- Expanded content: chronological timeline -->
    <div
      v-show="expanded"
      class="mt-1 px-3 py-2 rounded-xl border th-surface th-border text-xs space-y-2"
    >
      <template v-for="(item, idx) in timeline" :key="idx">
        <!-- Thinking chunk -->
        <div v-if="item.type === 'thinking'" class="flex gap-2 items-start rounded-xl bg-primary/5 border border-primary/10 px-3 py-2">
          <Sparkles class="w-3 h-3 text-primary/70 shrink-0 mt-0.5" />
          <div class="th-text-secondary whitespace-pre-wrap leading-relaxed flex-1 min-w-0 break-words">
            {{ item.content }}
          </div>
        </div>

        <!-- Tool call -->
        <div v-else-if="item.type === 'tool_call'">
        <Collapsible
          :open="isToolExpanded(item.tool)"
          class="rounded-xl border overflow-hidden transition-colors"
          :class="statusClasses(item.tool.status)"
        >
          <!-- Tool header (always visible, clickable) -->
          <CollapsibleTrigger as-child @click="toggleTool(item.tool.id)">
            <button class="w-full flex items-center gap-2 px-2.5 py-2 text-left">
              <component
                :is="iconForStatus(item.tool.status)"
                class="w-3.5 h-3.5 shrink-0"
                :class="{
                  'animate-spin': item.tool.status === 'running'
                }"
              />
              <span class="font-medium truncate flex-1">
                {{ toolDisplayName(item.tool) }}
              </span>
              <span
                class="text-[11px] px-1.5 py-0.5 rounded-full border shrink-0"
                :class="statusClasses(item.tool.status)"
              >
                {{ statusLabel(item.tool.status) }}
              </span>
              <span
                v-if="item.tool.duration"
                class="text-[11px] opacity-70 shrink-0"
              >
                {{ item.tool.duration }}
              </span>
              <ChevronRight
                class="w-3.5 h-3.5 shrink-0 opacity-60 transition-transform"
                :class="{ 'rotate-90': isToolExpanded(item.tool) }"
              />
            </button>
          </CollapsibleTrigger>

          <!-- Tool details (expandable) -->
          <CollapsibleContent>
            <div class="px-2.5 pb-2.5 space-y-2">
              <!-- Input -->
              <div v-if="item.tool.arguments" class="space-y-1">
                <div class="flex items-center gap-1 text-[11px] opacity-70 uppercase tracking-wider">
                  <Terminal class="w-3 h-3" />
                  <span>Input</span>
                </div>
                <div
                  class="font-mono text-[11px] bg-black/5 dark:bg-white/5 rounded-md p-2 whitespace-pre-wrap break-all max-h-40 overflow-auto leading-relaxed"
                >
                  {{ item.tool.arguments }}
                </div>
              </div>

              <!-- Output / Error -->
              <div v-if="item.tool.result" class="space-y-1">
                <div class="flex items-center gap-1 text-[11px] uppercase tracking-wider"
                  :class="item.tool.status === 'error' ? 'text-destructive opacity-90' : 'opacity-70'"
                >
                  <ArrowRight class="w-3 h-3" />
                  <span>{{ item.tool.status === 'error' ? 'Error' : 'Output' }}</span>
                </div>
                <div
                  class="font-mono text-[11px] rounded-md p-2 whitespace-pre-wrap break-words max-h-60 overflow-auto leading-relaxed"
                  :class="item.tool.status === 'error'
                    ? 'bg-destructive/10 text-destructive'
                    : 'bg-black/5 dark:bg-white/5 th-text-secondary'
                  "
                >
                  {{ item.tool.result }}
                </div>
              </div>
            </div>
          </CollapsibleContent>
        </Collapsible>

        <!-- Child agents spawned by this call, rendered as a tree below it -->
        <div
          v-if="item.tool.name === 'spawn_subagents' && subagentsForTool(item.tool).length > 0"
          class="ml-3 mt-1"
        >
          <SubagentThoughtsPanel
            :subagents="subagentsForTool(item.tool)"
            :phase="subagentPhase || 'completed'"
          />
        </div>
        </div>
      </template>

      <!-- Fallback: if no timeline but has old-format thinking -->
      <div v-if="timeline.length === 0 && hasThinking" class="flex gap-2 items-start rounded-xl bg-primary/5 border border-primary/10 px-3 py-2">
        <Sparkles class="w-3 h-3 text-primary/70 shrink-0 mt-0.5" />
        <div class="th-text-secondary whitespace-pre-wrap leading-relaxed flex-1 min-w-0 break-words">
          {{ message.thinking }}
        </div>
      </div>

      <!-- subagent_* 协议已实现：当 spawn_subagents 工具被调用时，网关转发子 agent 事件，以下渲染 -->
      <!-- Subagent results (fallback: no matching spawn_subagents call in the timeline) -->
      <SubagentThoughtsPanel
        v-if="orphanSubagents.length > 0"
        :subagents="orphanSubagents"
        :phase="subagentPhase || 'completed'"
      />
    </div>
  </div>
</template>
