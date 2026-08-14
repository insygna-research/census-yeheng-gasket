<script setup lang="ts">
import { computed, ref } from 'vue';
import { Shield, Terminal, Check, X } from 'lucide-vue-next';
import type { ApprovalRequest } from '@/types';

const props = defineProps<{
  request: ApprovalRequest | null;
}>();

const emit = defineEmits<{
  (e: 'respond', requestId: string, approved: boolean, remember: boolean): void;
}>();

const remember = ref(false);

const isOpen = computed(() => props.request !== null);

const handleAllow = () => {
  if (!props.request) return;
  emit('respond', props.request.id, true, remember.value);
  remember.value = false;
};

const handleDeny = () => {
  if (!props.request) return;
  emit('respond', props.request.id, false, remember.value);
  remember.value = false;
};

const formatArgs = (args: string | undefined) => {
  if (!args) return '(no arguments)';
  try {
    const parsed = JSON.parse(args);
    return JSON.stringify(parsed, null, 2);
  } catch {
    return args;
  }
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
      <div v-if="isOpen" class="fixed inset-0 z-50 flex items-center justify-center p-4">
        <!-- Backdrop -->
        <div class="absolute inset-0 bg-black/50 backdrop-blur-sm" @click="handleDeny" />

        <!-- Dialog -->
        <div
          class="relative w-full max-w-md bg-popover border border-border rounded-2xl shadow-2xl p-6 space-y-4 animate-in zoom-in-95 duration-200"
        >
          <!-- Header -->
          <div class="flex items-center gap-3">
            <div class="w-10 h-10 rounded-xl bg-amber-500/10 flex items-center justify-center shrink-0">
              <Shield class="w-5 h-5 text-amber-500" />
            </div>
            <div>
              <h3 class="text-sm font-semibold text-foreground">Operation Request</h3>
              <p class="text-xs text-muted-foreground">
                The agent wants to execute a sensitive tool
              </p>
            </div>
          </div>

          <!-- Details -->
          <div v-if="request" class="space-y-3">
            <div class="flex items-center gap-2 text-xs">
              <span class="px-2 py-0.5 rounded-md bg-primary/10 text-primary font-medium">
                {{ request.tool_name }}
              </span>
              <span v-if="request.description" class="text-muted-foreground">
                {{ request.description }}
              </span>
            </div>

            <div v-if="request.arguments" class="space-y-1">
              <div class="flex items-center gap-1.5 text-[11px] uppercase tracking-wider text-muted-foreground">
                <Terminal class="w-3 h-3" />
                <span>Arguments</span>
              </div>
              <div class="font-mono text-[11px] bg-black/5 dark:bg-white/5 rounded-lg p-3 whitespace-pre-wrap break-all max-h-40 overflow-auto leading-relaxed text-foreground">
                {{ formatArgs(request.arguments) }}
              </div>
            </div>
          </div>

          <!-- Remember checkbox -->
          <label class="flex items-center gap-2 text-xs text-muted-foreground cursor-pointer hover:text-foreground transition-colors">
            <input
              v-model="remember"
              type="checkbox"
              class="rounded border-border bg-background text-primary focus:ring-primary"
            />
            <span>Remember this decision for similar operations</span>
          </label>

          <!-- Actions -->
          <div class="flex gap-2 pt-1">
            <button
              @click="handleDeny"
              class="flex-1 flex items-center justify-center gap-1.5 px-4 py-2.5 rounded-xl border border-border bg-background text-foreground text-xs font-medium hover:bg-accent transition-colors"
            >
              <X class="w-3.5 h-3.5" />
              Deny
            </button>
            <button
              @click="handleAllow"
              class="flex-1 flex items-center justify-center gap-1.5 px-4 py-2.5 rounded-xl bg-primary text-primary-foreground text-xs font-medium hover:bg-primary/90 transition-colors shadow-sm"
            >
              <Check class="w-3.5 h-3.5" />
              Allow
            </button>
          </div>
        </div>
      </div>
    </Transition>
  </Teleport>
</template>
