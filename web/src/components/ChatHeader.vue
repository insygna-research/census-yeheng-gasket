<script setup lang="ts">
import { Button } from '@/components/ui/button';
import { Menu as HeadlessMenu, MenuButton, MenuItem, MenuItems } from '@headlessui/vue';
import { computed } from 'vue';
import { Cpu, Loader2, Moon, MoreVertical, Palette, RotateCcw, Sun, Trash2, Check } from 'lucide-vue-next';
import { useTheme, type ThemeHue, type MarkdownStyle } from '../composables/useTheme';
import type { ContextStats, WatermarkInfo } from '../types';

const props = defineProps<{
  isConnected: boolean;
  sessionStatus: string;
  showReconnectButton: boolean;
  contextStats?: ContextStats;
  watermarkInfo?: WatermarkInfo;
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
</script>

<template>
  <header class="py-3 px-5 th-header-bg border-b th-border flex justify-between items-center shrink-0">
    <div class="flex items-center gap-3">
      <div class="w-9 h-9 rounded-full bg-primary flex items-center justify-center">
        <svg class="w-5 h-5 text-primary-foreground" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <rect x="3" y="3" width="18" height="18" rx="2" />
          <line x1="3" y1="9" x2="21" y2="9" />
          <line x1="9" y1="21" x2="9" y2="9" />
        </svg>
      </div>
      <div>
        <div class="text-sm font-semibold th-text">Model</div>
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

      <HeadlessMenu as="div" class="relative">
        <MenuButton as="button" class="p-2 rounded-md th-hover th-text-muted hover:th-text transition-colors">
          <MoreVertical class="w-4 h-4" />
        </MenuButton>
        <transition
          enter-active-class="transition duration-100 ease-out"
          enter-from-class="transform scale-95 opacity-0"
          enter-to-class="transform scale-100 opacity-100"
          leave-active-class="transition duration-75 ease-in"
          leave-from-class="transform scale-100 opacity-100"
          leave-to-class="transform scale-95 opacity-0"
        >
          <MenuItems class="absolute right-0 top-10 z-30 w-44 origin-top-right rounded-lg bg-popover border border-border shadow-lg focus:outline-none py-1">
            <div v-if="watermarkInfo" class="px-3 py-1.5 text-[11px] th-text-dim">
              Watermark: {{ watermarkInfo.watermark }}/{{ watermarkInfo.max_sequence }}
            </div>
            <MenuItem v-if="contextStats" v-slot="{ active }">
              <button @click="emit('compact')" :disabled="isCompacting" :class="[active ? 'bg-accent' : '', 'group flex w-full items-center px-3 py-2 text-xs th-text-secondary disabled:opacity-50']">
                <Loader2 v-if="isCompacting" class="w-3.5 h-3.5 mr-2 animate-spin th-text-dim" />
                <Cpu v-else class="w-3.5 h-3.5 mr-2 th-text-dim" />
                {{ isCompacting ? 'Compressing...' : 'Compress Context' }}
              </button>
            </MenuItem>
            <MenuItem v-slot="{ active }">
              <button @click="emit('clear-history')" :class="[active ? 'bg-accent' : '', 'group flex w-full items-center px-3 py-2 text-xs th-text-secondary']">
                <Trash2 class="w-3.5 h-3.5 mr-2 th-text-dim" />
                Clear History
              </button>
            </MenuItem>
          </MenuItems>
        </transition>
      </HeadlessMenu>

      <HeadlessMenu as="div" class="relative">
        <MenuButton as="button" class="p-2 rounded-md th-hover th-text-muted hover:th-text transition-colors">
          <Palette class="w-4 h-4" />
        </MenuButton>
        <transition
          enter-active-class="transition duration-100 ease-out"
          enter-from-class="transform scale-95 opacity-0"
          enter-to-class="transform scale-100 opacity-100"
          leave-active-class="transition duration-75 ease-in"
          leave-from-class="transform scale-100 opacity-100"
          leave-to-class="transform scale-95 opacity-0"
        >
          <MenuItems class="absolute right-0 top-10 z-30 w-44 origin-top-right rounded-lg bg-popover border border-border shadow-lg focus:outline-none py-1">
            <!-- Mode -->
            <div class="px-3 py-1.5 text-[11px] font-semibold th-text-muted uppercase tracking-wider">Mode</div>
            <MenuItem v-slot="{ active }">
              <button
                @click="setMode('light')"
                :class="[active ? 'bg-accent' : '', 'group flex w-full items-center px-3 py-2 text-xs th-text-secondary']"
              >
                <Sun class="w-3.5 h-3.5 mr-2 th-text-dim" />
                <span class="flex-1 text-left">Light</span>
                <Check v-if="mode === 'light'" class="w-3 h-3 th-text-muted shrink-0" />
              </button>
            </MenuItem>
            <MenuItem v-slot="{ active }">
              <button
                @click="setMode('dark')"
                :class="[active ? 'bg-accent' : '', 'group flex w-full items-center px-3 py-2 text-xs th-text-secondary']"
              >
                <Moon class="w-3.5 h-3.5 mr-2 th-text-dim" />
                <span class="flex-1 text-left">Dark</span>
                <Check v-if="mode === 'dark'" class="w-3 h-3 th-text-muted shrink-0" />
              </button>
            </MenuItem>
            <div class="my-1 border-t border-border" />
            <!-- Hue -->
            <div class="px-3 py-1.5 text-[11px] font-semibold th-text-muted uppercase tracking-wider">Hue</div>
            <MenuItem v-for="h in hues" :key="h" v-slot="{ active }">
              <button
                @click="setHue(h)"
                :class="[active ? 'bg-accent' : '', 'group flex w-full items-center px-3 py-2 text-xs th-text-secondary']"
              >
                <span class="w-3 h-3 rounded-full mr-2 shrink-0" :class="hueMeta[h].dot" />
                <span class="flex-1 text-left">{{ hueMeta[h].label }}</span>
                <Check v-if="hue === h" class="w-3 h-3 th-text-muted shrink-0" />
              </button>
            </MenuItem>
            <div class="my-1 border-t border-border" />
            <!-- Markdown Style -->
            <div class="px-3 py-1.5 text-[11px] font-semibold th-text-muted uppercase tracking-wider">Markdown</div>
            <MenuItem v-for="s in markdownStyles" :key="s" v-slot="{ active }">
              <button
                @click="setMarkdownStyle(s)"
                :class="[active ? 'bg-accent' : '', 'group flex w-full items-center px-3 py-2 text-xs th-text-secondary']"
              >
                <span class="flex-1 text-left">{{ mdStyleMeta[s].label }}</span>
                <Check v-if="markdownStyle === s" class="w-3 h-3 th-text-muted shrink-0" />
              </button>
            </MenuItem>
          </MenuItems>
        </transition>
      </HeadlessMenu>
    </div>
  </header>
</template>
