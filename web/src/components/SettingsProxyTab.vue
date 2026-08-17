<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { Ban, Check, X } from 'lucide-vue-next';
import { Input } from '@/components/ui/input';
import { readString, storageKeys, writeString } from '@/lib/storage';
import { isTauri } from '@/lib/platform';

const emit = defineEmits<{ (e: 'close'): void }>();

const url = ref('');
const error = ref('');

// Reload the stored value each time the tab is shown (the parent v-if remounts it).
onMounted(() => {
  url.value = readString(storageKeys.proxy, '');
  error.value = '';
});

const PROXY_RE = /^(https?|socks5h?):\/\/\S+$/i;
const trimmed = computed(() => url.value.trim());

const save = async () => {
  const value = trimmed.value;
  if (value && !PROXY_RE.test(value)) {
    error.value = 'URL must start with http://, https://, socks5:// or socks5h://';
    return;
  }
  // Authoritative validation lives in the backend (it accepts/rejects the
  // same URLs set_tool_proxy would); this regex is only the browser-mode
  // fallback. A rejected URL must surface here, not in a console.warn.
  if (isTauri && value) {
    try {
      await invoke('validate_proxy', { url: value });
    } catch (e) {
      error.value = String(e);
      return;
    }
  }
  writeString(storageKeys.proxy, value);
  emit('close');
};

const disable = () => {
  writeString(storageKeys.proxy, '');
  emit('close');
};
</script>

<template>
  <div class="space-y-4">
    <p class="text-xs text-muted-foreground">Routes fetch / web_search tool traffic.</p>

    <!-- Input -->
    <div class="space-y-1.5">
      <Input
        v-model="url"
        placeholder="socks5://127.0.0.1:1080"
        class="font-mono text-xs"
        @keyup.enter="save"
      />
      <p v-if="error" class="text-[11px] text-destructive">{{ error }}</p>
      <p v-else class="text-[11px] text-muted-foreground">
        Schemes: http, https, socks5, socks5h. Credentials: user:pass@host.
      </p>
    </div>

    <p v-if="!isTauri" class="text-[11px] text-amber-500">
      Browser mode: the proxy only takes effect in the desktop app.
    </p>
    <p v-else class="text-[11px] text-muted-foreground">
      Applies to the next tool call — no restart needed. Disable falls back to CONGA_TOOL_PROXY if set.
    </p>

    <!-- Actions -->
    <div class="flex gap-2 pt-1">
      <button
        @click="emit('close')"
        class="flex-1 flex items-center justify-center gap-1.5 px-4 py-2.5 rounded-xl border border-border bg-background text-foreground text-xs font-medium hover:bg-accent transition-colors"
      >
        <X class="w-3.5 h-3.5" />
        Cancel
      </button>
      <button
        @click="disable"
        class="flex-1 flex items-center justify-center gap-1.5 px-4 py-2.5 rounded-xl border border-border bg-background text-foreground text-xs font-medium hover:bg-accent transition-colors"
      >
        <Ban class="w-3.5 h-3.5" />
        Disable
      </button>
      <button
        @click="save"
        class="flex-1 flex items-center justify-center gap-1.5 px-4 py-2.5 rounded-xl bg-primary text-primary-foreground text-xs font-medium hover:bg-primary/90 transition-colors shadow-sm"
      >
        <Check class="w-3.5 h-3.5" />
        Save
      </button>
    </div>
  </div>
</template>
