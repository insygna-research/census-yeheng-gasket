import { defineStore } from 'pinia';
import { ref, computed } from 'vue';
import { deleteSession, fetchSessionList, renameSession } from '@/lib/backend';
import type { Chat, Message, MessageStatus, SubagentState, TurnSummary } from '@/types';

/** Collision-resistant local id: `prefix_timestamp_random`. One generator
 * for every locally-created entity (messages, tool calls, chats, traces). */
export const makeId = (prefix?: string) => {
  const core = Date.now().toString(36) + '_' + Math.random().toString(36).slice(2, 11);
  return prefix ? `${prefix}_${core}` : core;
};

export const useChatStore = defineStore('chat', () => {
  // The backend's session store (~/.conga/sessions via list_sessions) is
  // the single source of truth for the chat list; nothing is persisted
  // client-side. The list starts empty and fills from syncFromBackend()
  // (called on mount); message bodies hydrate lazily per chat from
  // fetchSessionMessages.
  const chats = ref<Chat[]>([]);
  const activeChatId = ref<string>('');

  const activeChat = computed(() => chats.value.find(c => c.id === activeChatId.value));
  const activeMessages = computed(() => activeChat.value?.messages || []);

  const getChat = (chatId: string) => chats.value.find(c => c.id === chatId);

  const createChat = () => {
    const newChat: Chat = {
      id: makeId('chat'),
      name: 'New Chat',
      messages: [
        { id: makeId(), role: 'system', content: 'Connected to conga Gateway', timestamp: Date.now() }
      ],
      updatedAt: Date.now()
    };
    chats.value.unshift(newChat);
    activeChatId.value = newChat.id;
    return newChat.id;
  };

  const deleteChat = (id: string) => {
    // The backend's on-disk store is authoritative; a failed delete means
    // the session simply reappears on the next sync — honest, no local
    // shadow list.
    deleteSession(id).then(ok => {
      if (!ok) console.warn('Failed to delete session on backend:', id);
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
      // Persist to the backend's meta.json sidecar — the only durable copy;
      // a failure just means the name stays in-memory for this run.
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
      id: makeId(),
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

  const setTurnSummary = (chatId: string, messageId: string, summary: TurnSummary) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg) msg.turnSummary = summary;
  };

  const abortToolCalls = (chatId: string) => {
    const chat = getChat(chatId);
    if (!chat) return;
    // Same lookup as abortSubagents: the LAST BOT message, not the literal
    // last message — after a retry the literal last message may be a user
    // message and running tool calls would silently survive.
    const lastBot = [...chat.messages].reverse().find(m => m.role === 'bot');
    if (lastBot?.toolCalls) {
      lastBot.toolCalls.forEach(tc => {
        if (tc.status === 'running') tc.status = 'error';
      });
    }
  };

  const abortSubagents = (chatId: string, messageId: string) => {
    const chat = getChat(chatId);
    if (!chat) return;
    const msg = chat.messages.find(m => m.id === messageId);
    if (msg?.subagents) {
      msg.subagents.forEach(s => {
        if (s.status === 'running') {
          s.status = 'error';
          s.error = 'Cancelled';
          s.endTime = Date.now();
        }
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


  // Hydrate the chat list from the backend (discovery: CLI-created,
  // other-device, or this-device sessions — the on-disk store is the only
  // record). Names come from the meta.json sidecar; messages hydrate lazily
  // on activate via fetchSessionMessages.
  const syncFromBackend = async () => {
    try {
      const sessions = await fetchSessionList();
      for (const s of sessions) {
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
    // Initialize after the backend list lands (called on mount): an empty
    // store gets a fresh local chat; otherwise activate the newest.
    if (chats.value.length === 0) {
      createChat();
    } else if (!activeChatId.value) {
      activeChatId.value = chats.value[0].id;
    }
  };

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
    setTurnSummary,
    abortToolCalls,
    abortSubagents,
    ensureSubagents,
    pushSubagent,
    updateSubagent,
    syncFromBackend
  };
});
