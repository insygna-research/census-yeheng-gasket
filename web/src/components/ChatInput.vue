<script setup lang="ts">
import { Button } from '@/components/ui/button';
import { Send, Square, Terminal } from 'lucide-vue-next';
import { computed, nextTick, onMounted, ref } from 'vue';
import { isTauri } from '@/lib/platform';

const props = defineProps<{
  isConnected: boolean;
  sessionStatus: string;
  isThinking: boolean;
  isReceiving: boolean;
}>();

const emit = defineEmits<{
  (e: 'send', text: string): void;
  (e: 'stop'): void;
}>();

const inputRef = ref<HTMLTextAreaElement | null>(null);
const inputValue = ref('');

// ── Slash Command Completer ─────────────────────────────────

interface SlashCommand {
  name: string;
  description: string;
  aliases?: string[];
}

const commands = ref<SlashCommand[]>([]);

const fetchCommands = async () => {
  // Tauri: no gateway HTTP to query; the backend handles /clear and /help
  // in-process (chat.rs). Keep this in sync with the slash-command handler.
  if (isTauri) {
    commands.value = [
      { name: 'clear', description: 'Clear the current session' },
      { name: 'help', description: 'Show available commands' },
    ];
    return;
  }
  try {
    const apiUrl = import.meta.env.VITE_API_URL || 'http://localhost:3000';
    const res = await fetch(`${apiUrl}/api/commands`);
    if (res.ok) {
      const data = await res.json();
      commands.value = Array.isArray(data) ? data : [];
    }
  } catch (e) {
    // Silently fail - completer just won't show commands
    console.warn('Failed to fetch commands:', e);
  }
};

onMounted(() => {
  fetchCommands();
});

const showCompleter = ref(false);
const selectedIndex = ref(0);

const filteredCommands = computed(() => {
  const text = inputValue.value;
  if (!text.startsWith('/')) return [];
  const query = text.slice(1).toLowerCase();
  if (query.includes(' ')) return [];
  return commands.value.filter(cmd =>
    cmd.name.toLowerCase().startsWith(query) ||
    cmd.aliases?.some(a => a.toLowerCase().startsWith(query))
  );
});

const openCompleter = () => {
  if (filteredCommands.value.length > 0) {
    showCompleter.value = true;
    selectedIndex.value = 0;
  } else {
    showCompleter.value = false;
  }
};

const closeCompleter = () => {
  showCompleter.value = false;
  selectedIndex.value = 0;
};

const selectCommand = (cmd: SlashCommand) => {
  inputValue.value = `/${cmd.name} `;
  showCompleter.value = false;
  nextTick(() => {
    inputRef.value?.focus();
    autoResize();
    // Move cursor to end
    const el = inputRef.value;
    if (el) {
      el.selectionStart = el.selectionEnd = el.value.length;
    }
  });
};

const handleKeydown = (event: KeyboardEvent) => {
  if (showCompleter.value && filteredCommands.value.length > 0) {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      selectedIndex.value = (selectedIndex.value + 1) % filteredCommands.value.length;
      return;
    }
    if (event.key === 'ArrowUp') {
      event.preventDefault();
      selectedIndex.value = (selectedIndex.value - 1 + filteredCommands.value.length) % filteredCommands.value.length;
      return;
    }
    if (event.key === 'Enter' || event.key === 'Tab') {
      event.preventDefault();
      selectCommand(filteredCommands.value[selectedIndex.value]);
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      closeCompleter();
      return;
    }
  }

  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    submit();
  }
};

const handleInput = () => {
  autoResize();
  const text = inputValue.value;
  if (text.startsWith('/') && !text.slice(1).includes(' ')) {
    openCompleter();
  } else {
    closeCompleter();
  }
};

const autoResize = () => {
  const el = inputRef.value;
  if (!el) return;
  el.style.height = 'auto';
  el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
};

const submit = () => {
  const text = inputValue.value.trim();
  if (!text || !props.isConnected || props.sessionStatus === 'sending') return;
  emit('send', text);
  inputValue.value = '';
  closeCompleter();
  nextTick(() => {
    inputRef.value?.focus();
    if (inputRef.value) inputRef.value.style.height = 'auto';
  });
};
</script>

<template>
  <div class="p-4 bg-transparent shrink-0">
    <div class="w-full relative">
      <!-- Slash Command Completer Dropdown -->
      <Transition
        enter-active-class="transition-all duration-150 ease-out"
        enter-from-class="opacity-0 translate-y-1 scale-95"
        enter-to-class="opacity-100 translate-y-0 scale-100"
        leave-active-class="transition-all duration-100 ease-in"
        leave-from-class="opacity-100 translate-y-0 scale-100"
        leave-to-class="opacity-0 translate-y-1 scale-95"
      >
        <div
          v-if="showCompleter && filteredCommands.length > 0"
          class="absolute bottom-full left-0 right-0 mb-2 z-50"
        >
          <div class="th-surface border th-border rounded-xl shadow-2xl overflow-hidden max-h-64 flex flex-col">
            <div class="px-3 py-1.5 text-[11px] font-semibold th-text-muted uppercase tracking-wider border-b th-border">
              Commands
            </div>
            <div class="overflow-y-auto">
              <button
                v-for="(cmd, idx) in filteredCommands"
                :key="cmd.name"
                @click="selectCommand(cmd)"
                @mouseenter="selectedIndex = idx"
                class="w-full text-left px-3 py-2 flex items-center gap-2.5 transition-colors"
                :class="idx === selectedIndex ? 'bg-primary/10' : 'hover:bg-accent/50'"
              >
                <Terminal class="w-3.5 h-3.5 th-text-dim shrink-0" />
                <div class="flex-1 min-w-0">
                  <div class="flex items-center gap-1.5">
                    <span class="text-xs font-medium th-text">/{{ cmd.name }}</span>
                    <span v-if="cmd.aliases?.length" class="text-[11px] th-text-dim">
                      ({{ cmd.aliases.map(a => '/' + a).join(', ') }})
                    </span>
                  </div>
                  <div class="text-[11px] th-text-secondary truncate">{{ cmd.description }}</div>
                </div>
              </button>
            </div>
          </div>
        </div>
      </Transition>

      <div class="flex items-end th-input-bg border th-border rounded-2xl p-2 transition-all"
        :class="{
          'focus-within:border-primary/50 focus-within:ring-1 focus-within:ring-primary/20': sessionStatus === 'idle' || sessionStatus === 'disconnected',
          'border-primary/30 ring-1 ring-primary/20': sessionStatus === 'receiving' || sessionStatus === 'sending'
        }">
        <textarea ref="inputRef" v-model="inputValue" @keydown="handleKeydown" @input="handleInput"
          :placeholder="sessionStatus === 'receiving' ? 'AI is processing...' : 'Type a message...'"
          :disabled="!isConnected || sessionStatus === 'receiving' || sessionStatus === 'sending'"
          autofocus rows="1"
          class="flex-1 overflow-x-hidden border-0 bg-transparent shadow-none focus:outline-none focus:ring-0 th-text px-3 py-2.5 disabled:opacity-50 disabled:cursor-not-allowed resize-none min-h-[40px] max-h-[200px]"></textarea>

        <Button v-if="sessionStatus === 'receiving' || isThinking" @click="emit('stop')"
          class="w-9 h-9 rounded-xl bg-destructive/80 hover:bg-destructive text-white shrink-0 ml-2 transition-all" size="icon" title="Stop generating">
          <Square class="w-3.5 h-3.5 fill-current" />
        </Button>
        <Button v-else @click="submit" :disabled="!inputValue.trim() || !isConnected || sessionStatus === 'sending'"
          class="w-9 h-9 rounded-xl text-white shrink-0 ml-2 transition-all"
          :class="{ 'bg-primary hover:opacity-90': sessionStatus === 'idle', 'bg-muted cursor-not-allowed': sessionStatus !== 'idle' }"
          size="icon">
          <Send class="w-4 h-4" />
        </Button>
      </div>
      <div class="flex items-center justify-center text-[11px] th-text-dim mt-2 px-1">
        <span>Shift+Enter for new line</span>
      </div>
    </div>
  </div>
</template>
