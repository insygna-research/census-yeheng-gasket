import { computed, reactive, ref, watch } from 'vue';
import { useChatStore, makeId } from '@/stores/chatStore';
import { useIMWebSocket } from '@/hooks/useIMWebSocket';
import { useTauriChat } from '@/hooks/useTauriChat';
import { isTauri } from '@/lib/platform';
import { backendBaseUrl, fetchSessionMessages } from '@/lib/backend';
import { parseWsMessage } from '@/types';
import type { ApprovalRequest, ContextStats, Message, SubagentState, WsMessage } from '@/types';
import { notifyTurnComplete } from '@/lib/notifications';
import { invoke } from '@tauri-apps/api/core';

export function useChatSession(chatId: { value: string }) {
  const chatStore = useChatStore();

  const isThinking = ref(false);
  const isSending = ref(false);
  const isReceiving = ref(false);
  const isCompacting = ref(false);

  const subagentPhase = ref<'idle' | 'running' | 'synthesizing' | 'completed'>('idle');
  const subagentTimers = ref<Record<string, ReturnType<typeof setTimeout>>>({});
  const SUBAGENT_TIMEOUT_MS = 300_000; // 5 minutes client-side timeout as a safety net

  const pendingApprovals = ref<Map<string, ApprovalRequest>>(new Map());

  // Explicit identity of the current turn's bot message. Guessing "last
  // message is the bot's" breaks on retry (the retried turn's events would
  // append to a stale bot bubble); track it instead. Null between turns —
  // the first bot-ish WS event of a turn creates and records the message.
  const currentBotMessageId = ref<string | null>(null);

  const errorBanner = ref<string | null>(null);
  let errorBannerTimer: ReturnType<typeof setTimeout> | null = null;

  const contextStats = computed(() => chatStore.activeChat?.contextStats);

  const usageColor = computed(() => {
    const pct = contextStats.value?.usage_percent || 0;
    if (pct < 80) return 'bg-primary';
    if (pct < 100) return 'bg-amber-500';
    return 'bg-destructive';
  });

  type SessionStatus = 'disconnected' | 'idle' | 'sending' | 'receiving';

  const showError = (message: string) => {
    errorBanner.value = message;
    if (errorBannerTimer) clearTimeout(errorBannerTimer);
    errorBannerTimer = setTimeout(() => { errorBanner.value = null; }, 8000);
  };

  const dismissError = () => {
    errorBanner.value = null;
    if (errorBannerTimer) clearTimeout(errorBannerTimer);
  };

  // ── Subagent handling ───────────────────────────────────────

  const getBotSubagent = (botMsg: Message, id: string): SubagentState | undefined =>
    botMsg.subagents?.find(sa => sa.id === id);

  const handleSubagentStarted = (msg: Extract<WsMessage, { type: 'subagent_started' }>, botMsg: Message) => {
    chatStore.pushSubagent(chatId.value, botMsg.id, {
      id: msg.id,
      index: msg.index,
      task: msg.task,
      status: 'running',
      timeline: [],
      toolCalls: [],
      toolCount: 0,
      startTime: Date.now(),
    });
    if (subagentPhase.value !== 'running' && subagentPhase.value !== 'synthesizing') {
      subagentPhase.value = 'running';
    }

    // Client-side timeout: if backend never sends completed/error, force-finish the task.
    // Re-lookup by ids inside the callback — never capture the Message object:
    // the timer fires minutes later, and correctness must not depend on the
    // store mutating that exact object in place.
    const chatIdNow = chatId.value;
    const botIdNow = botMsg.id;
    if (subagentTimers.value[msg.id]) clearTimeout(subagentTimers.value[msg.id]);
    subagentTimers.value[msg.id] = setTimeout(() => {
      const bot = chatStore.getChat(chatIdNow)?.messages.find(m => m.id === botIdNow);
      const s = bot?.subagents?.find(sa => sa.id === msg.id);
      if (s && s.status === 'running') {
        chatStore.updateSubagent(chatIdNow, botIdNow, msg.id, {
          status: 'error',
          error: 'Timed out',
          endTime: Date.now(),
        });
        if (bot) checkAndFinalizeSubagents(bot);
      }
      delete subagentTimers.value[msg.id];
    }, SUBAGENT_TIMEOUT_MS);
  };

  const handleSubagentThinking = (msg: Extract<WsMessage, { type: 'subagent_thinking' }>, botMsg: Message) => {
    const s = getBotSubagent(botMsg, msg.id);
    if (s) {
      // Consecutive thinking chunks merge into the current block; a tool
      // call in between starts a new block, so the timeline preserves the
      // real arrival order (think → tool → think → tool …).
      const timeline = [...s.timeline];
      const last = timeline[timeline.length - 1];
      if (last && last.kind === 'thinking') {
        timeline[timeline.length - 1] = { ...last, text: last.text + msg.content };
      } else {
        timeline.push({ kind: 'thinking', text: msg.content });
      }
      chatStore.updateSubagent(chatId.value, botMsg.id, msg.id, { timeline });
    }
  };

  const handleSubagentContent = (msg: Extract<WsMessage, { type: 'subagent_content' }>, botMsg: Message) => {
    const s = getBotSubagent(botMsg, msg.id);
    if (s) {
      chatStore.updateSubagent(chatId.value, botMsg.id, msg.id, { content: (s.content || '') + msg.content });
    }
  };

  const handleSubagentToolStart = (msg: Extract<WsMessage, { type: 'subagent_tool_start' }>, botMsg: Message) => {
    const s = getBotSubagent(botMsg, msg.id);
    if (s) {
      const toolCall = {
        id: makeId(),
        name: msg.name,
        arguments: msg.arguments,
        status: 'running' as const,
        output: null,
        startTime: Date.now(),
      };
      chatStore.updateSubagent(chatId.value, botMsg.id, msg.id, {
        toolCalls: [...s.toolCalls, toolCall],
        toolCount: s.toolCount + 1,
        timeline: [...s.timeline, { kind: 'tool', toolId: toolCall.id }],
      });
    }
  };

  const handleSubagentToolEnd = (msg: Extract<WsMessage, { type: 'subagent_tool_end' }>, botMsg: Message) => {
    const s = getBotSubagent(botMsg, msg.id);
    if (s && s.toolCalls.length > 0) {
      // Sub-agents execute tools serially, so the newest running call with
      // this name is the one that just finished.
      const target = [...s.toolCalls].reverse().find(t => t.name === msg.name && t.status === 'running');
      if (target) {
        const elapsed = target.startTime ? Date.now() - target.startTime : 0;
        const newTools = s.toolCalls.map(t =>
          t === target
            ? { ...t, status: 'complete' as const, output: msg.output || null, duration: (elapsed / 1000).toFixed(1) + 's' }
            : t
        );
        chatStore.updateSubagent(chatId.value, botMsg.id, msg.id, { toolCalls: newTools });
      }
    }
  };

  const checkAndFinalizeSubagents = (botMsg: Message) => {
    const subs = botMsg.subagents;
    if (subs && subs.length > 0 && subs.every(s => s.status !== 'running')) {
      subagentPhase.value = 'completed';
    }
  };

  const handleSubagentCompleted = (msg: Extract<WsMessage, { type: 'subagent_completed' }>, botMsg: Message) => {
    if (subagentTimers.value[msg.id]) {
      clearTimeout(subagentTimers.value[msg.id]);
      delete subagentTimers.value[msg.id];
    }
    chatStore.updateSubagent(chatId.value, botMsg.id, msg.id, {
      status: 'completed',
      summary: msg.summary,
      toolCount: msg.tool_count,
      endTime: Date.now(),
    });
    checkAndFinalizeSubagents(botMsg);
  };

  const handleSubagentError = (msg: Extract<WsMessage, { type: 'subagent_error' }>, botMsg: Message) => {
    if (subagentTimers.value[msg.id]) {
      clearTimeout(subagentTimers.value[msg.id]);
      delete subagentTimers.value[msg.id];
    }
    chatStore.updateSubagent(chatId.value, botMsg.id, msg.id, {
      status: 'error',
      error: msg.error,
      endTime: Date.now(),
    });
    checkAndFinalizeSubagents(botMsg);
  };

  // ── WebSocket message processing ────────────────────────────

  const processWebSocketMessageInner = (msg: WsMessage, botMsg: Message) => {
    switch (msg.type) {
      case 'thinking':
        isThinking.value = true;
        chatStore.appendToMessage(chatId.value, botMsg.id, msg.content, 'thinking');
        break;
      case 'tool_start':
        isThinking.value = true;
        chatStore.ensureToolCalls(chatId.value, botMsg.id);
        const toolId = msg.tool_call_id || makeId();
        chatStore.pushToolCall(chatId.value, botMsg.id, {
          id: toolId,
          name: msg.name,
          arguments: msg.arguments || '',
          status: 'running',
          result: null,
          startTime: Date.now()
        });
        break;
      case 'tool_end':
        isThinking.value = true;
        const toolCalls = chatStore.activeMessages.find(m => m.id === botMsg.id)?.toolCalls;
        // Match by tool_call_id when the gateway provides it (exact); fall back to
        // newest-running-by-name for older gateways. end-without-start (denied/
        // timeout/cancel) still has no matching entry — append a standalone errored
        // entry below with the real id.
        const runningTool = toolCalls
          ? (msg.tool_call_id
              ? toolCalls.find(t => t.id === msg.tool_call_id && t.status === 'running')
              : [...toolCalls].reverse().find(t => t.name === msg.name && t.status === 'running'))
          : undefined;
        if (runningTool) {
          const updates: { status: 'error' | 'complete'; result: string; duration?: string } = { status: msg.error ? 'error' : 'complete', result: msg.error || msg.output || '' };
          if (runningTool.startTime) {
            updates.duration = ((Date.now() - runningTool.startTime) / 1000).toFixed(1);
          }
          chatStore.updateToolCall(chatId.value, botMsg.id, runningTool.id, updates);
        } else {
          chatStore.ensureToolCalls(chatId.value, botMsg.id);
          chatStore.pushToolCall(chatId.value, botMsg.id, {
            id: msg.tool_call_id || makeId(),
            name: msg.name || 'unknown',
            arguments: '',
            status: 'error',
            result: msg.error || msg.output || '',
            startTime: Date.now()
          });
        }
        break;
      case 'content':
      case 'text':
        isThinking.value = false;
        chatStore.appendToMessage(chatId.value, botMsg.id, msg.content, 'content');
        break;
      case 'error':
        isThinking.value = false;
        isReceiving.value = false;
        subagentPhase.value = 'idle';
        chatStore.abortSubagents(chatId.value, botMsg.id);
        Object.values(subagentTimers.value).forEach(clearTimeout);
        subagentTimers.value = {};
        showError(msg.content || msg.message || 'An error occurred');
        break;
      case 'done':
        // Turn complete: release the tracked bot message.
        currentBotMessageId.value = null;
        isThinking.value = false;
        // 回合结束（含审批超时/连接关闭后的 done）：清理残留审批弹窗。
        // 网关保证 done 排在全部 subagent 事件之后（单一有序通道），
        // 到达这里时子面板必然已收尾。
        pendingApprovals.value.clear();
        isReceiving.value = false;
        // Turn summary: `done_with_summary` carries cumulative tokens + elapsed.
        // Absent for slash-command replies and pre-summary turns.
        if (msg.usage_in != null && msg.usage_out != null && msg.elapsed_ms != null) {
          chatStore.setTurnSummary(chatId.value, botMsg.id, {
            usageIn: msg.usage_in,
            usageOut: msg.usage_out,
            elapsedMs: msg.elapsed_ms,
          });
        }
        fetchContext();
        // Notify only for replies with actual content — a slash-command
        // echo ("(session cleared)") is not worth a system notification.
        if (botMsg.content.trim()) {
          notifyTurnComplete(
            chatStore.getChat(chatId.value)?.name || 'Conga',
            botMsg.content
          );
        }
        break;
      case 'busy':
        // 发送时回合已在进行（竞态/打断）：只提示，不动会话状态——
        // 正在流式的回复和子面板不能被清掉。
        // 但 processWebSocketMessage 顶部已把 isReceiving 置 true，busy
        // 不是流式回合的一部分，不复位会让输入框永久锁死（sendMessage
        // 的守卫从此静默拦截一切消息）。若确有回合在收尾，它的后续事件
        // 会重新置位、done 会再清掉。
        isReceiving.value = false;
        showError(msg.message || 'The agent is busy processing a request');
        break;
      // subagent_*: live events forwarded by the gateway's single ordered
      // wire channel (event_map::subagent_event_to_ws) while a
      // spawn_subagents fan-out runs.
      case 'subagent_all_started':
        subagentPhase.value = 'running';
        break;
      case 'subagent_started':
        handleSubagentStarted(msg, botMsg);
        break;
      case 'subagent_thinking':
        handleSubagentThinking(msg, botMsg);
        break;
      case 'subagent_content':
        handleSubagentContent(msg, botMsg);
        break;
      case 'subagent_tool_start':
        handleSubagentToolStart(msg, botMsg);
        break;
      case 'subagent_tool_end':
        handleSubagentToolEnd(msg, botMsg);
        break;
      case 'subagent_completed':
        handleSubagentCompleted(msg, botMsg);
        break;
      case 'subagent_error':
        handleSubagentError(msg, botMsg);
        break;
      case 'subagent_synthesizing':
        subagentPhase.value = 'synthesizing';
        setTimeout(() => { subagentPhase.value = 'completed' }, 300);
        break;
      case 'approval_request':
        pendingApprovals.value.set(msg.id, {
          id: msg.id,
          tool_name: msg.tool_name,
          description: msg.description,
          arguments: msg.arguments,
        });
        break;
    }
  };

  const processWebSocketMessage = (raw: unknown) => {
    const parsed = parseWsMessage(raw);
    if (!parsed) return; // unknown/absent discriminant — drop, nothing sane to do

    isSending.value = false;
    isReceiving.value = true;

    let botMsg: Message | null = null;
    if (currentBotMessageId.value) {
      const tracked = chatStore.activeMessages.find(m => m.id === currentBotMessageId.value);
      if (tracked) botMsg = tracked;
    }
    if (!botMsg) {
      botMsg = chatStore.getOrCreateBotMessage(chatId.value);
      if (botMsg) currentBotMessageId.value = botMsg.id;
    }
    if (!botMsg) return;
    processWebSocketMessageInner(parsed, botMsg);
  };

  const handleMessage = (data: string) => {
    try {
      const msg = JSON.parse(data);
      processWebSocketMessage(msg);
    } catch (e) {
      isThinking.value = false;
      isSending.value = false;
      console.error('Malformed gateway frame:', e, data.slice(0, 200));
      showError('Received a malformed message from the server');
    }
  };

  // Transport selection: inside the desktop shell the chat runs in-process
  // over Tauri IPC; in a plain browser (dev) it keeps using the gateway's
  // WebSocket. Both transports deliver the same wire events to handleMessage.
  const { isConnected, showReconnectButton, connect, manualReconnect, send } =
    isTauri
      ? useTauriChat(computed(() => chatId.value), handleMessage)
      : useIMWebSocket(computed(() => chatId.value), handleMessage);

  const sessionStatus = computed<SessionStatus>(() => {
    if (!isConnected.value) return 'disconnected';
    if (isSending.value) return 'sending';
    if (isReceiving.value || isThinking.value) return 'receiving';
    return 'idle';
  });

  // ── Context API ─────────────────────────────────────────────

  const fetchContext = async () => {
    try {
      if (isTauri) {
        // Tauri: invoke the in-process get_context command (mirrors the
        // gateway's GET /api/sessions/:id/context). Same
        // { context_stats } JSON shape.
        const data = await invoke<{ context_stats?: ContextStats }>('get_context', { sessionId: chatId.value });
        if (data?.context_stats) {
          chatStore.setContextStats(chatId.value, data.context_stats);
        }
        return;
      }
      const res = await fetch(`${backendBaseUrl()}/api/sessions/${encodeURIComponent(chatId.value)}/context`);
      const data = await res.json();
      if (res.ok && data.context_stats) {
        chatStore.setContextStats(chatId.value, data.context_stats);
      }
    } catch (e) {
      console.error('Fetch context failed:', e);
    }
  };

  // Hydrate the transcript from the backend's authoritative store.
  const fetchMessages = async () => {
    // Hydrating over a live stream would clobber in-flight state — skip.
    if (isSending.value || isReceiving.value || isThinking.value) return;
    const targetId = chatId.value;
    const messages = await fetchSessionMessages(targetId);
    if (!messages) return; // 404 for local-only chats, or request failed
    if (chatId.value !== targetId) return; // user switched chats mid-fetch
    if (isSending.value || isReceiving.value || isThinking.value) return;
    chatStore.setMessages(targetId, messages);
    // Backend sessions have no names — derive one from the first user message.
    const chat = chatStore.getChat(targetId);
    if (chat && (chat.name === 'New Chat' || chat.name.startsWith('Session ('))) {
      const firstUser = messages.find(m => m.role === 'user');
      if (firstUser) {
        const name = firstUser.content.slice(0, 50) + (firstUser.content.length > 50 ? '...' : '');
        chatStore.renameChat(targetId, name);
      }
    }
  };

  // Auto-fetch context and transcript when connection is established or restored
  watch(isConnected, (connected, prev) => {
    if (connected && !prev) {
      fetchContext();
      fetchMessages();
    }
  });

  // Hydrate on chat switch and on mount
  watch(chatId, () => fetchMessages(), { immediate: true });

  const forceCompact = async () => {
    if (isCompacting.value) return;
    isCompacting.value = true;
    try {
      // Tauri has no compaction endpoint; refreshing context is enough.
      if (isTauri) {
        await fetchContext();
        return;
      }
      const res = await fetch(`${backendBaseUrl()}/api/sessions/${encodeURIComponent(chatId.value)}/context/compact`, { method: 'POST' });
      const data = await res.json();
      if (res.ok && data.context_stats) {
        chatStore.setContextStats(chatId.value, data.context_stats);
      }
    } catch (e) {
      console.error('Force compact failed:', e);
    } finally {
      isCompacting.value = false;
    }
  };

  // ── Public interface ────────────────────────────────────────

  const stopGenerating = () => {
    send(JSON.stringify({ type: 'cancel' }));
    isThinking.value = false;
    isReceiving.value = false;
    isSending.value = false;
    pendingApprovals.value.clear();
    chatStore.abortToolCalls(chatId.value);
    // Clear sub-agent state: cancel aborts sub-agent tasks, so Synthesizing
    // never arrives — without this the panels would spin until the 5-minute
    // client timeout.
    subagentPhase.value = 'idle';
    // Read-only lookup: getOrCreate would fabricate an empty bot bubble when
    // the user hits stop with no reply in flight.
    const lastBotMsg = [...chatStore.activeMessages].reverse().find(m => m.role === 'bot');
    if (lastBotMsg) chatStore.abortSubagents(chatId.value, lastBotMsg.id);
    Object.values(subagentTimers.value).forEach(clearTimeout);
    subagentTimers.value = {};
  };

  const sendApprovalResponse = (requestId: string, approved: boolean, remember: boolean = false) => {
    send(JSON.stringify({
      type: 'approval_response',
      request_id: requestId,
      approved,
      remember,
    }));
    pendingApprovals.value.delete(requestId);
  };


  const sendMessage = (text: string) => {
    // 接收期间一律禁发（含子 agent 运行中）：后端在回合内不会接受新
    // message，发送只会被静默丢弃——之前允许 running 期间发送是个
    // 契约错觉。
    if (!text.trim() || !isConnected.value || isSending.value || isReceiving.value) return false;

    const msgId = makeId();
    chatStore.appendMessage(chatId.value, {
      id: msgId,
      role: 'user',
      content: text,
      timestamp: Date.now(),
      status: 'sending'
    });
    // New turn: the next bot events belong to a fresh bot message, not
    // whatever bot bubble happens to be last.
    currentBotMessageId.value = null;

    isSending.value = true;
    const payload = JSON.stringify({
      type: 'message',
      content: text,
      trace_id: makeId('trace'),
    });
    // `send` reports failure by return value (both transports), never by
    // throwing — drive the message status from it.
    if (send(payload)) {
      chatStore.updateMessageStatus(chatId.value, msgId, 'sent');
      // Refresh context after sending since backend may have updated token usage
      fetchContext();
      return true;
    }
    isSending.value = false;
    chatStore.updateMessageStatus(chatId.value, msgId, 'error');
    return false;
  };

  const retryMessage = (msgId: string, content: string) => {
    if (!isConnected.value) return;
    chatStore.updateMessageStatus(chatId.value, msgId, 'sending');
    // Retried turn gets its own bot message; do not append to a stale one.
    currentBotMessageId.value = null;
    const payload = JSON.stringify({
      type: 'message',
      content,
      trace_id: makeId('trace'),
    });
    if (send(payload)) {
      chatStore.updateMessageStatus(chatId.value, msgId, 'sent');
    } else {
      chatStore.updateMessageStatus(chatId.value, msgId, 'error');
    }
  };

  return reactive({
    // Status
    isConnected,
    isThinking,
    isSending,
    isReceiving,
    isCompacting,
    sessionStatus,
    showReconnectButton,
    // Context
    contextStats,
    usageColor,
    // Subagents
    subagentPhase,
    // Approvals
    pendingApprovals,
    // Error
    errorBanner,
    // Actions
    connect,
    manualReconnect,
    sendMessage,
    retryMessage,
    stopGenerating,
    sendApprovalResponse,
    fetchContext,
    fetchMessages,
    forceCompact,
    dismissError,
  });
}
