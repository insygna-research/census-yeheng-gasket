<script setup lang="ts">
import { Button } from '@/components/ui/button';
import {
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuPortal,
  DropdownMenuRoot,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from 'radix-vue';
import { computed, ref } from 'vue';
import { Cpu, Loader2, Moon, MoreVertical, Palette, RotateCcw, Settings, Sun, Trash2, Check } from 'lucide-vue-next';
import SettingsDialog from './SettingsDialog.vue';
import { useTheme, type ThemeHue, type MarkdownStyle } from '../composables/useTheme';
import type { ContextStats } from '@/types';

const props = defineProps<{
  chatTitle: string;
  isConnected: boolean;
  sessionStatus: string;
  showReconnectButton: boolean;
  contextStats?: ContextStats;
  usageColor: string;
  isCompacting: boolean;
}>();

const emit = defineEmits<{
  (e: 'reconnect'): void;
  (e: 'compact'): void;
  (e: 'clear-history'): void;
}>();

const { mode, hue, setMode, setHue, hues, markdownStyle, setMarkdownStyle, markdownStyles } = useTheme();

const hueMeta: Record<ThemeHue, { label: string; dot: string }> = {
  zinc:    { label: 'Zinc',    dot: 'bg-zinc-500' },
  blue:    { label: 'Blue',    dot: 'bg-blue-500' },
  rose:    { label: 'Rose',    dot: 'bg-rose-500' },
  emerald: { label: 'Emerald', dot: 'bg-emerald-500' },
  amber:   { label: 'Amber',   dot: 'bg-amber-500' },
  violet:  { label: 'Violet',  dot: 'bg-violet-500' },
};

const mdStyleMeta: Record<MarkdownStyle, { label: string }> = {
  classic: { label: 'Classic' },
  github:  { label: 'GitHub' },
};

const statusText = computed(() => {
  if (props.sessionStatus === 'disconnected') return 'Disconnected';
  if (props.sessionStatus === 'sending') return 'Sending...';
  if (props.sessionStatus === 'receiving') return 'Thinking...';
  return 'Online';
});

const menuContentClass =
  'z-30 w-44 rounded-lg bg-popover border border-border shadow-lg py-1 will-change-[transform,opacity] ' +
  'data-[state=open]:animate-in data-[state=open]:fade-in-0 data-[state=open]:zoom-in-95';
const menuItemClass =
  'flex w-full items-center px-3 py-2 text-xs th-text-secondary outline-none cursor-pointer select-none data-[highlighted]:bg-accent data-[disabled]:opacity-50 data-[disabled]:pointer-events-none';
const menuLabelClass =
  'px-3 py-1.5 text-[11px] font-semibold th-text-muted uppercase tracking-wider';

const showSettingsDialog = ref(false);
</script>

<template>
  <header class="py-3 px-5 th-header-bg border-b th-border flex justify-between items-center shrink-0" data-tauri-drag-region>
    <div class="flex items-center gap-3 min-w-0">
      <div class="w-9 h-9 rounded-full bg-primary flex items-center justify-center shrink-0">
        <svg class="w-5 h-5 text-primary-foreground" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <rect x="3" y="3" width="18" height="18" rx="2" />
          <line x1="3" y1="9" x2="21" y2="9" />
          <line x1="9" y1="21" x2="9" y2="9" />
        </svg>
      </div>
      <div class="min-w-0">
        <div class="text-sm font-semibold th-text truncate max-w-[40vw]">{{ chatTitle }}</div>
        <div class="text-[11px] th-text-muted flex items-center gap-1.5">
          <span class="w-1.5 h-1.5 rounded-full" :class="isConnected ? 'bg-primary' : 'bg-destructive'" />
          <Loader2 v-if="sessionStatus === 'sending' || sessionStatus === 'receiving'" class="w-3 h-3 animate-spin" />
          <span>{{ statusText }}</span>
        </div>
      </div>
    </div>

    <div class="flex items-center gap-2">
      <!-- Context stats inline -->
      <div v-if="contextStats" class="hidden md:flex items-center gap-2 mr-1">
        <div class="text-[11px] th-text-secondary font-medium whitespace-nowrap">
          Context: {{ contextStats.usage_percent.toFixed(1) }}%
        </div>
        <div class="w-20 lg:w-28 h-1.5 bg-muted rounded-full overflow-hidden">
          <div class="h-full rounded-full transition-all duration-500" :class="usageColor" :style="{ width: Math.min(contextStats.usage_percent, 100) + '%' }" />
        </div>
      </div>

      <Button v-if="showReconnectButton" variant="outline" size="sm" @click="emit('reconnect')"
        class="text-primary border-primary/30 hover:bg-primary/10 text-xs h-8">
        <RotateCcw class="w-3.5 h-3.5 mr-1.5" />
        Reconnect
      </Button>

      <!-- Session actions -->
      <DropdownMenuRoot>
        <DropdownMenuTrigger as-child>
          <button class="p-2 rounded-md th-hover th-text-muted hover:th-text transition-colors" title="Session actions">
            <MoreVertical class="w-4 h-4" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuPortal>
          <DropdownMenuContent align="end" :side-offset="6" :class="menuContentClass">
            <DropdownMenuItem v-if="contextStats" :disabled="isCompacting" :class="menuItemClass" @select="emit('compact')">
              <Loader2 v-if="isCompacting" class="w-3.5 h-3.5 mr-2 animate-spin th-text-dim" />
              <Cpu v-else class="w-3.5 h-3.5 mr-2 th-text-dim" />
              {{ isCompacting ? 'Compressing...' : 'Compress Context' }}
            </DropdownMenuItem>
            <DropdownMenuItem :class="menuItemClass" @select="emit('clear-history')">
              <Trash2 class="w-3.5 h-3.5 mr-2 th-text-dim" />
              Clear History
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenuPortal>
      </DropdownMenuRoot>

      <!-- Settings (model, prompt, proxy) -->
      <button
        class="p-2 rounded-md th-hover th-text-muted hover:th-text transition-colors"
        title="Settings"
        @click="showSettingsDialog = true"
      >
        <Settings class="w-4 h-4" />
      </button>
      <DropdownMenuRoot>
        <DropdownMenuTrigger as-child>
          <button class="p-2 rounded-md th-hover th-text-muted hover:th-text transition-colors" title="Appearance">
            <Palette class="w-4 h-4" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuPortal>
          <DropdownMenuContent align="end" :side-offset="6" :class="menuContentClass">
            <DropdownMenuLabel :class="menuLabelClass">Mode</DropdownMenuLabel>
            <DropdownMenuItem :class="menuItemClass" @select="setMode('light')">
              <Sun class="w-3.5 h-3.5 mr-2 th-text-dim" />
              <span class="flex-1 text-left">Light</span>
              <Check v-if="mode === 'light'" class="w-3 h-3 th-text-muted shrink-0" />
            </DropdownMenuItem>
            <DropdownMenuItem :class="menuItemClass" @select="setMode('dark')">
              <Moon class="w-3.5 h-3.5 mr-2 th-text-dim" />
              <span class="flex-1 text-left">Dark</span>
              <Check v-if="mode === 'dark'" class="w-3 h-3 th-text-muted shrink-0" />
            </DropdownMenuItem>
            <DropdownMenuSeparator class="my-1 h-px bg-border" />
            <DropdownMenuLabel :class="menuLabelClass">Hue</DropdownMenuLabel>
            <DropdownMenuItem v-for="h in hues" :key="h" :class="menuItemClass" @select="setHue(h)">
              <span class="w-3 h-3 rounded-full mr-2 shrink-0" :class="hueMeta[h].dot" />
              <span class="flex-1 text-left">{{ hueMeta[h].label }}</span>
              <Check v-if="hue === h" class="w-3 h-3 th-text-muted shrink-0" />
            </DropdownMenuItem>
            <DropdownMenuSeparator class="my-1 h-px bg-border" />
            <DropdownMenuLabel :class="menuLabelClass">Markdown</DropdownMenuLabel>
            <DropdownMenuItem v-for="s in markdownStyles" :key="s" :class="menuItemClass" @select="setMarkdownStyle(s)">
              <span class="flex-1 text-left">{{ mdStyleMeta[s].label }}</span>
              <Check v-if="markdownStyle === s" class="w-3 h-3 th-text-muted shrink-0" />
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenuPortal>
      </DropdownMenuRoot>

      <SettingsDialog :open="showSettingsDialog" @close="showSettingsDialog = false" />
    </div>
  </header>
</template>
