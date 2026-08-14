import { defineStore } from 'pinia';
import { ref, computed } from 'vue';
import { deleteSession, fetchSessionList, renameSession } from '@/lib/backend';
import { readJSON, storageKeys, writeJSON } from '@/lib/storage';
import type { Chat, Message, MessageStatus, SubagentState } from '@/types';

const LEGACY_KEY = 'gasket_sessions';

/** localStorage keeps chat metadata only; messages live on the backend. */
interface ChatMeta {
  id: string;
  name: string;
  updatedAt: number;
}

const loadLocalChats = (): Chat[] => {
  const meta = readJSON<ChatMeta[]>(storageKeys.chatsMeta, []);
  if (meta.length > 0) {
    return meta.map(m => ({ ...m, messages: [] }));
  }

  // One-time migration from the full-transcript store: keep names and drop
  // message bodies — the backend's events.jsonl is the authoritative copy.
  const legacy = readJSON<(ChatMeta & { messages?: unknown })[]>(storageKeys.legacyChats, []);
  if (legacy.length > 0) {
    localStorage.removeItem(storageKeys.legacyChats);
    localStorage.removeItem(LEGACY_KEY);
    return legacy.map(c => ({
      id: c.id,
      name: c.name,
      updatedAt: c.updatedAt || Date.now(),
      messages: [],
    }));
  }

  localStorage.removeItem(LEGACY_KEY);
  return [];
};

const loadHiddenIds = (): Set<string> =>
  new Set(readJSON<string[]>(storageKeys.hiddenSessions, []));

export const useChatStore = defineStore('chat', () => {
  const chats = ref<Chat[]>(loadLocalChats());
  const hiddenIds = ref<Set<string>>(loadHiddenIds());
  const activeChatId = ref<string>('');

  const activeChat = computed(() => chats.value.find(c => c.id === activeChatId.value));
  const activeMessages = computed(() => activeChat.value?.messages || []);

  const getChat = (chatId: string) => chats.value.find(c => c.id === chatId);

  const createChat = () => {
    const newChat: Chat = {
      id: 'chat_' + Date.now() + '_' + Math.random().toString(36).substr(2, 9),
      name: 'New Chat',
      messages: [
        { id: Date.now().toString(), role: 'system', content: 'Connected to gasket Gateway', timestamp: Date.now() }
      ],
      updatedAt: Date.now()
    };
    chats.value.unshift(newChat);
    activeChatId.value = newChat.id;
    return newChat.id;
  };

  const deleteChat = (id: string) => {
    // Backend delete is authoritative; when the gateway is unreachable (or
    // too old to have the endpoint) fall back to the local hidden list so
    // the session stays off the list after the next sync.
    deleteSession(id).then(ok => {
      if (!ok) {
        hiddenIds.value.add(id);
        writeJSON(storageKeys.hiddenSessions, [...hiddenIds.value]);
      }
    });
    chats.value = chats.value.filter(c => c.id !== id);
    if (activeChatId.value === id) {
      activeChatId.value = chats.value.length > 0 ? chats.value[0].id : '';
    }
    if (chats.value.length === 0) {
      createChat();
    }
  };

  const setActiveChat = (id: string) => {
    activeChatId.value = id;
  };

  const renameChat = (id: string, name: string) => {
    const chat = chats.value.find(c => c.id === id);
    const trimmed = name.trim();
    if (chat && trimmed) {
      chat.name = trimmed;
      // Persist backend-side so the name survives devices and localStorage
      // loss; a failure just means the name stays local-only.
      renameSession(id, trimmed).then(ok => {
        if (!ok) console.warn('Failed to sync session name to backend:', id);
      });
    }
  };

  const getOrCreateBotMessage = (chatId: string): Message | null => {
    const chat = getChat(chatId);
    if (!chat) return null;

    const lastMsg = chat.messages[chat.messages.length - 1];
    if (lastMsg && lastMsg.role === 'bot') {
      return lastMsg;
    }

    const newBotMsg: Message = {
      id: Date.now().toString(),
      role: 'bot',
      content: '',
      timestamp: Date.now()
    };
    chat.messages.push(newBotMsg);
    chat.updatedAt = Date.now();
    return newBotMsg;
  };

  const appendMessage = (chatId: string, message: Message) => {
    const chat = getChat(chatId);
    if (!chat) return;
    chat.messages.push(message);
    chat.updatedAt = Date.now();
    if (chat.name === 'New Chat' && message.role === 'user' && message.content) {
      const sanitizedName = message.content
        .replace(/[\x00-\x1f\x7f]/g, '')
        .replace(/\s+/g, ' ')
        .trim()
        .slice(0, 50);
      chat.name = sanitizedName + (message.content.length > 50 ? '...' : '');
    }
  };

  const updateMessageStatus = (chatId: string, messageId: string, status: MessageStatus) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg) {
      msg.status = status;
      chat.updatedAt = Date.now();
    }
  };

  const appendToMessage = (chatId: string, messageId: string, content: string, field: 'content' | 'thinking' = 'content') => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg) {
      if (field === 'thinking') {
        // Ensure newline separation between chunks to prevent concatenation
        const prefix = msg.thinking && !msg.thinking.endsWith('\n') ? '\n' : '';
        msg.thinking = (msg.thinking || '') + prefix + content;
        if (!msg.thinkingChunks) msg.thinkingChunks = [];
        msg.thinkingChunks.push({ content, timestamp: Date.now() });
      } else {
        msg.content = (msg.content || '') + content;
      }
      chat.updatedAt = Date.now();
    }
  };

  const updateMessage = (chatId: string, messageId: string, updates: Partial<Message>) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg) {
      Object.assign(msg, updates);
      chat.updatedAt = Date.now();
    }
  };

  const ensureToolCalls = (chatId: string, messageId: string) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg && !msg.toolCalls) {
      msg.toolCalls = [];
    }
  };

  const pushToolCall = (chatId: string, messageId: string, toolCall: any) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg) {
      if (!msg.toolCalls) msg.toolCalls = [];
      const tc = { ...toolCall, startTime: toolCall.startTime || Date.now() };
      msg.toolCalls.push(tc);
      chat.updatedAt = Date.now();
    }
  };

  const updateToolCall = (chatId: string, messageId: string, toolId: string, updates: any) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg && msg.toolCalls) {
      const tool = msg.toolCalls.find(t => t.id === toolId);
      if (tool) {
        Object.assign(tool, updates);
        chat.updatedAt = Date.now();
      }
    }
  };

  const clearChatMessages = (chatId: string) => {
    const chat = getChat(chatId);
    if (!chat) return;
    chat.messages = [];
    chat.updatedAt = Date.now();
  };

  /** Replace a chat's transcript wholesale (backend hydration). */
  const setMessages = (chatId: string, messages: Message[]) => {
    const chat = getChat(chatId);
    if (!chat) return;
    chat.messages = messages;
  };

  const setContextStats = (chatId: string, stats: any) => {
    const chat = getChat(chatId);
    if (chat) chat.contextStats = stats;
  };

  const setWatermarkInfo = (chatId: string, info: any) => {
    const chat = getChat(chatId);
    if (chat) chat.watermarkInfo = info;
  };

  const abortToolCalls = (chatId: string) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const lastMsg = chat.messages[chat.messages.length - 1];
    if (lastMsg && lastMsg.role === 'bot' && lastMsg.toolCalls) {
      lastMsg.toolCalls.forEach(tc => {
        if (tc.status === 'running') tc.status = 'error';
      });
    }
  };

  const ensureSubagents = (chatId: string, messageId: string) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg && !msg.subagents) {
      msg.subagents = [];
    }
  };

  const pushSubagent = (chatId: string, messageId: string, subagent: SubagentState) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg) {
      if (!msg.subagents) msg.subagents = [];
      msg.subagents.push(subagent);
      chat.updatedAt = Date.now();
    }
  };

  const updateSubagent = (chatId: string, messageId: string, subagentId: string, updates: Partial<SubagentState>) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg && msg.subagents) {
      const subagent = msg.subagents.find(s => s.id === subagentId);
      if (subagent) {
        Object.assign(subagent, updates);
        chat.updatedAt = Date.now();
      }
    }
  };


  // Sync sessions from backend (discovery: CLI-created, other-device, or
  // lost localStorage). The backend is authoritative for the session list;
  // local meta only contributes names. Messages hydrate lazily on activate
  // via fetchSessionMessages.
  const syncFromBackend = async () => {
    try {
      const sessions = await fetchSessionList();
      for (const s of sessions) {
        if (hiddenIds.value.has(s.id)) continue;
        const existing = chats.value.find(c => c.id === s.id);
        if (existing) {
          existing.updatedAt = Math.max(existing.updatedAt, s.mtime || 0);
          // A backend-side name (renamed on any device) wins; otherwise the
          // local name stands.
          if (s.name) existing.name = s.name;
        } else {
          chats.value.push({
            id: s.id,
            name: s.name || `Session (${s.msg_count} msgs)`,
            messages: [],
            updatedAt: s.mtime || Date.now(),
          });
        }
      }
    } catch (e) {
      console.error('syncFromBackend failed:', e);
    }
  };

  // Initialize
  if (chats.value.length === 0) {
    createChat();
  } else if (!activeChatId.value) {
    activeChatId.value = chats.value[0].id;
  }

  return {
    chats,
    activeChatId,
    activeChat,
    activeMessages,
    createChat,
    deleteChat,
    setActiveChat,
    renameChat,
    getChat,
    getOrCreateBotMessage,
    appendMessage,
    updateMessageStatus,
    appendToMessage,
    updateMessage,
    ensureToolCalls,
    pushToolCall,
    updateToolCall,
    clearChatMessages,
    setMessages,
    setContextStats,
    setWatermarkInfo,
    abortToolCalls,
    ensureSubagents,
    pushSubagent,
    updateSubagent,
    syncFromBackend
  };
});
