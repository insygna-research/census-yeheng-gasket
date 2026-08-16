<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { Ban, Check, Globe, X } from 'lucide-vue-next';
import { Input } from '@/components/ui/input';
import { readString, storageKeys, writeString } from '@/lib/storage';
import { isTauri } from '@/lib/platform';

const props = defineProps<{ open: boolean }>();
const emit = defineEmits<{ (e: 'close'): void }>();

const url = ref('');
const error = ref('');

// Reload the stored value each time the dialog opens.
watch(
  () => props.open,
  (open) => {
    if (open) {
      url.value = readString(storageKeys.proxy, '');
      error.value = '';
    }
  }
);

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
  <Teleport to="body">
    <Transition
      enter-active-class="transition-all duration-200 ease-out"
      leave-active-class="transition-all duration-150 ease-in"
      enter-from-class="opacity-0"
      leave-to-class="opacity-0"
    >
      <div v-if="open" class="fixed inset-0 z-50 flex items-center justify-center p-4">
        <!-- Backdrop -->
        <div class="absolute inset-0 bg-black/50 backdrop-blur-sm" @click="emit('close')" />

        <!-- Dialog -->
        <div
          class="relative w-full max-w-md bg-popover border border-border rounded-2xl shadow-2xl p-6 space-y-4 animate-in zoom-in-95 duration-200"
        >
          <!-- Header -->
          <div class="flex items-center gap-3">
            <div class="w-10 h-10 rounded-xl bg-primary/10 flex items-center justify-center shrink-0">
              <Globe class="w-5 h-5 text-primary" />
            </div>
            <div>
              <h3 class="text-sm font-semibold text-foreground">Network Proxy</h3>
              <p class="text-xs text-muted-foreground">
                Routes fetch / web_search tool traffic
              </p>
            </div>
          </div>

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
            Applies to the next tool call — no restart needed. Disable falls back to GASKET_TOOL_PROXY if set.
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
      </div>
    </Transition>
  </Teleport>
</template>
