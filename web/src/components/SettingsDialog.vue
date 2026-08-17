<script setup lang="ts">
import { ref, watch } from 'vue';
import { Check, Cpu, Loader2, X } from 'lucide-vue-next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { renderMarkdownBlock } from '@/lib/markdown';
import { fetchEnvSettings, saveEnvSettings } from '@/lib/backend';
import type { EnvSettingsView, LlmSettingsGroup } from '@/types';

const props = defineProps<{ open: boolean }>();
const emit = defineEmits<{ (e: 'close'): void }>();

/** One editable group; `enabled` maps to null (cleared) vs present. */
interface EditableGroup {
  enabled: boolean;
  baseUrl: string;
  apiKey: string;
  model: string;
  api: 'openai' | 'anthropic';
  apiKeySet: boolean;
  apiKeyHint: string;
}

const emptyGroup = (): EditableGroup => ({
  enabled: false,
  baseUrl: '',
  apiKey: '',
  model: '',
  api: 'openai',
  apiKeySet: false,
  apiKeyHint: '',
});

const llm = ref<EditableGroup>(emptyGroup());
const fast = ref<EditableGroup>(emptyGroup());
const systemPrompt = ref('');
const previewing = ref(false);
const previewHtml = ref('');
const error = ref('');
const saving = ref(false);
const adopt = (view: EnvSettingsView | null) => {
  const fromView = (g: EnvSettingsView['llm']): EditableGroup => {
    if (!g) return emptyGroup();
    return {
      enabled: true,
      baseUrl: g.baseUrl,
      apiKey: '',
      model: g.model,
      api: g.api === 'anthropic' ? 'anthropic' : 'openai',
      apiKeySet: g.apiKeySet,
      apiKeyHint: g.apiKeyHint,
    };
  };
  llm.value = fromView(view?.llm ?? null);
  fast.value = fromView(view?.fastLlm ?? null);
  systemPrompt.value = view?.systemPrompt ?? '';
};

// Reload the stored (masked) view each time the dialog opens.
watch(
  () => props.open,
  async open => {
    if (open) {
      error.value = '';
      previewing.value = false;
      previewHtml.value = '';
      const view = await fetchEnvSettings();
      if (view) {
        adopt(view);
      } else {
        llm.value = emptyGroup();
        fast.value = emptyGroup();
        systemPrompt.value = '';
      }
    }
  }
);

const toPayloadGroup = (g: EditableGroup): LlmSettingsGroup | null =>
  g.enabled
    ? { baseUrl: g.baseUrl.trim(), apiKey: g.apiKey.trim(), model: g.model.trim(), api: g.api }
    : null;

const validate = (): string => {
  const check = (name: string, g: EditableGroup): string => {
    if (!g.baseUrl.trim()) return `${name}: base URL is required`;
    if (!/^https?:\/\//.test(g.baseUrl.trim()))
      return `${name}: base URL must start with http:// or https://`;
    if (!g.model.trim()) return `${name}: model is required`;
    if (!g.apiKey.trim() && !g.apiKeySet)
      return `${name}: API key is required (no stored key to keep)`;
    return '';
  };
  if (llm.value.enabled) {
    const e = check('Main model', llm.value);
    if (e) return e;
  }
  if (fast.value.enabled) {
    const e = check('Fast model', fast.value);
    if (e) return e;
  }
  return '';
};

const save = async () => {
  error.value = validate();
  if (error.value) return;
  saving.value = true;
  const result = await saveEnvSettings({
    llm: toPayloadGroup(llm.value),
    fastLlm: toPayloadGroup(fast.value),
    systemPrompt: systemPrompt.value.trim(),
  });
  saving.value = false;
  if ('error' in result) {
    error.value = result.error;
    return;
  }
  adopt(result.view);
  emit('close');
};

// Toggle Edit/Preview; the preview re-renders on every toggle.
const togglePreview = () => {
  if (previewing.value) {
    previewing.value = false;
    return;
  }
  previewHtml.value = renderMarkdownBlock(systemPrompt.value);
  previewing.value = true;
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
          class="relative w-full max-w-md bg-popover border border-border rounded-2xl shadow-2xl p-6 space-y-4 animate-in zoom-in-95 duration-200 max-h-[85vh] overflow-auto"
        >
          <!-- Header -->
          <div class="flex items-center gap-3">
            <div class="w-10 h-10 rounded-xl bg-primary/10 flex items-center justify-center shrink-0">
              <Cpu class="w-5 h-5 text-primary" />
            </div>
            <div>
              <h3 class="text-sm font-semibold text-foreground">Model Settings</h3>
              <p class="text-xs text-muted-foreground">
                Overrides the server's env config; applies from the next message
              </p>
            </div>
          </div>

          <!-- Main LLM group -->
          <div class="space-y-3 rounded-xl border border-border p-3">
            <label class="flex items-center gap-2 text-xs font-medium text-foreground cursor-pointer">
              <input v-model="llm.enabled" type="checkbox" class="rounded border-border text-primary" />
              <span>Main model</span>
              <span class="text-muted-foreground font-normal">(overrides CONGA_LLM_*)</span>
            </label>
            <template v-if="llm.enabled">
              <Input v-model="llm.baseUrl" placeholder="Base URL (https://api.deepseek.com/v1)" class="text-xs" />
              <div class="flex gap-2">
                <Input v-model="llm.model" placeholder="Model (deepseek-chat)" class="text-xs" />
                <select
                  v-model="llm.api"
                  class="text-xs rounded-md border border-border bg-background px-2 shrink-0"
                >
                  <option value="openai">openai</option>
                  <option value="anthropic">anthropic</option>
                </select>
              </div>
              <div class="space-y-1">
                <Input
                  v-model="llm.apiKey"
                  type="password"
                  :placeholder="llm.apiKeySet ? `stored (${llm.apiKeyHint}) — leave blank to keep` : 'API key'"
                  class="text-xs"
                />
              </div>
            </template>
          </div>

          <!-- Fast LLM group -->
          <div class="space-y-3 rounded-xl border border-border p-3">
            <label class="flex items-center gap-2 text-xs font-medium text-foreground cursor-pointer">
              <input v-model="fast.enabled" type="checkbox" class="rounded border-border text-primary" />
              <span>Fast model (sub-agents)</span>
              <span class="text-muted-foreground font-normal">(overrides CONGA_FAST_LLM_*)</span>
            </label>
            <template v-if="fast.enabled">
              <Input v-model="fast.baseUrl" placeholder="Base URL" class="text-xs" />
              <div class="flex gap-2">
                <Input v-model="fast.model" placeholder="Model" class="text-xs" />
                <select
                  v-model="fast.api"
                  class="text-xs rounded-md border border-border bg-background px-2 shrink-0"
                >
                  <option value="openai">openai</option>
                  <option value="anthropic">anthropic</option>
                </select>
              </div>
              <Input
                v-model="fast.apiKey"
                type="password"
                :placeholder="fast.apiKeySet ? `stored (${fast.apiKeyHint}) — leave blank to keep` : 'API key'"
                class="text-xs"
              />
            </template>
          </div>

          <!-- Custom system prompt -->
          <div class="space-y-2 rounded-xl border border-border p-3">
            <div class="flex items-center justify-between gap-2">
              <div class="min-w-0">
                <p class="text-xs font-medium text-foreground">System prompt</p>
                <p class="text-[11px] text-muted-foreground truncate">
                  Replaces the built-in base instructions; project doc / skills / environment
                  stay appended. Empty = built-in.
                </p>
              </div>
              <div class="flex gap-1 shrink-0">
                <button
                  class="px-2 py-1 rounded-md text-[11px] th-hover th-text-muted hover:th-text"
                  :class="{ 'bg-accent th-text': previewing }"
                  title="Toggle markdown preview"
                  @click="togglePreview"
                >
                  {{ previewing ? 'Edit' : 'Preview' }}
                </button>
                <button
                  class="px-2 py-1 rounded-md text-[11px] th-hover th-text-muted hover:th-text"
                  title="Clear back to the built-in prompt"
                  @click="systemPrompt = ''; previewing = false"
                >
                  Reset
                </button>
              </div>
            </div>
            <textarea
              v-if="!previewing"
              v-model="systemPrompt"
              rows="8"
              spellcheck="false"
              placeholder="# Custom instructions (markdown)&#10;&#10;You are ..."
              class="w-full rounded-lg border border-border bg-background px-3 py-2 text-xs font-mono th-text resize-y min-h-[120px] focus:outline-none focus:ring-1 focus:ring-primary"
            />
            <div
              v-else
              class="prose prose-sm max-w-none rounded-lg border border-border bg-background px-3 py-2 text-xs overflow-auto min-h-[120px] max-h-64"
              v-html="previewHtml"
            />
            <p v-if="previewing && !systemPrompt.trim()" class="text-[11px] text-muted-foreground">
              (empty - the built-in prompt applies)
            </p>
          </div>

          <p v-if="error" class="text-xs text-destructive">{{ error }}</p>

          <!-- Actions -->
          <div class="flex gap-2 pt-1">
            <Button variant="outline" class="flex-1 h-8 text-xs" @click="emit('close')">
              <X class="w-3.5 h-3.5 mr-1" /> Cancel
            </Button>
            <Button class="flex-1 h-8 text-xs" :disabled="saving" @click="save">
              <Loader2 v-if="saving" class="w-3.5 h-3.5 mr-1 animate-spin" />
              <Check v-else class="w-3.5 h-3.5 mr-1" />
              Save
            </Button>
          </div>
        </div>
      </div>
    </Transition>
  </Teleport>
</template>
