import { computed, onUnmounted, reactive, ref, watch } from 'vue';
import { useChatStore, makeId } from '@/stores/chatStore';
import { useIMWebSocket } from '@/hooks/useIMWebSocket';
import { useTauriChat } from '@/hooks/useTauriChat';
import { isTauri } from '@/lib/platform';
import { fetchSessionMessages, gatewayFetch } from '@/lib/backend';
import { parseWsMessage } from '@/types';
import type { ApprovalRequest, ContextStats, Message, SubagentState, WsMessage } from '@/types';
import { notifyTurnComplete } from '@/lib/notifications';
import { invoke } from '@tauri-apps/api/core';

/**
 * Everything one in-flight turn owns. The composable is a singleton over the
 * transport but retargets as the user switches chats, so turn state is keyed
 * by session id: a turn streaming in chat A must never latch flags on chat B
 * (locked input, stop cancelling the wrong session).
 */
interface TurnState {
  isSending: boolean;
  isReceiving: boolean;
  isThinking: boolean;
  // Explicit identity of the current turn's bot message. Guessing "last
  // message is the bot's" breaks on retry (the retried turn's events would
  // append to a stale bot bubble); track it instead. Null between turns —
  // the first bot-ish WS event of a turn creates and records the message.
  currentBotMessageId: string | null;
  pendingApprovals: Map<string, ApprovalRequest>;
  subagentPhase: 'idle' | 'running' | 'synthesizing' | 'completed';
  subagentTimers: Record<string, ReturnType<typeof setTimeout>>;
}

const newTurnState = (): TurnState => ({
  isSending: false,
  isReceiving: false,
  isThinking: false,
  currentBotMessageId: null,
  pendingApprovals: new Map(),
  subagentPhase: 'idle',
  subagentTimers: {},
});

export function useChatSession(chatId: { value: string }) {
  const chatStore = useChatStore();

  const isCompacting = ref(false);

  const SUBAGENT_TIMEOUT_MS = 300_000; // 5 minutes client-side timeout as a safety net

  // Turn state keyed by session id. Values become reactive proxies when read
  // through the reactive Map. IDLE_TURN backs sessions with no entry yet so
  // the computeds below stay read-only (no get-or-create inside a computed).
  const turnStates = reactive(new Map<string, TurnState>());
  const IDLE_TURN = newTurnState();

  const turnState = (sid: string): TurnState => {
    let st = turnStates.get(sid);
    if (!st) {
      st = newTurnState();
      turnStates.set(sid, st);
    }
    return st;
  };

  const isTurnRunning = (st: TurnState) => st.isSending || st.isReceiving || st.isThinking;

  const isTurnBusy = (sid: string) => {
    const st = turnStates.get(sid);
    return !!st && isTurnRunning(st);
  };

  // The UI always observes the ACTIVE chat's entry; background sessions keep
  // their own state until their turn ends (or is reset below).
  const activeTurn = computed(() => turnStates.get(chatId.value) || IDLE_TURN);
  const isThinking = computed(() => activeTurn.value.isThinking);
  const isSending = computed(() => activeTurn.value.isSending);
  const isReceiving = computed(() => activeTurn.value.isReceiving);
  const pendingApprovals = computed(() => activeTurn.value.pendingApprovals);
  const subagentPhase = computed(() => activeTurn.value.subagentPhase);

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

  // ── Streaming delta batcher ────────────────────────────────
  // A fast stream delivers many deltas per frame; appending each one to the
  // store re-rendered the whole transcript per delta (quadratic cost over a
  // turn). Buffer consecutive content/thinking deltas and flush once per
  // animation frame — Vue coalesces the batched mutations into one render.
  // Every non-delta event flushes first so tool calls, subagent updates and
  // terminal events keep their exact arrival order.
  interface PendingDelta {
    sid: string;
    messageId: string;
    ops: { field: 'content' | 'thinking'; text: string }[];
  }
  const pendingDeltas = new Map<string, PendingDelta>();
  let deltaRafId: number | null = null;

  const flushDeltas = () => {
    deltaRafId = null;
    for (const delta of pendingDeltas.values()) {
      for (const op of delta.ops) {
        chatStore.appendToMessage(delta.sid, delta.messageId, op.text, op.field);
      }
    }
    pendingDeltas.clear();
  };

  const queueDelta = (sid: string, messageId: string, text: string, field: 'content' | 'thinking') => {
    // Ids never contain a newline, so this key cannot collide.
    const key = `${sid}\n${messageId}`;
    const entry = pendingDeltas.get(key) || { sid, messageId, ops: [] };
    entry.ops.push({ field, text });
    pendingDeltas.set(key, entry);
    if (deltaRafId === null) deltaRafId = requestAnimationFrame(flushDeltas);
  };

  // ── Subagent handling ───────────────────────────────────────

  const getBotSubagent = (botMsg: Message, id: string): SubagentState | undefined =>
    botMsg.subagents?.find(sa => sa.id === id);

  const handleSubagentStarted = (msg: Extract<WsMessage, { type: 'subagent_started' }>, botMsg: Message, sid: string, st: TurnState) => {
    chatStore.pushSubagent(sid, botMsg.id, {
      id: msg.id,
      index: msg.index,
      task: msg.task,
      status: 'running',
      timeline: [],
      toolCalls: [],
      toolCount: 0,
      startTime: Date.now(),
    });
    if (st.subagentPhase !== 'running' && st.subagentPhase !== 'synthesizing') {
      st.subagentPhase = 'running';
    }

    // Client-side timeout: if backend never sends completed/error, force-finish the task.
    // Re-lookup by ids inside the callback — never capture the Message object:
    // the timer fires minutes later, and correctness must not depend on the
    // store mutating that exact object in place.
    const botIdNow = botMsg.id;
    if (st.subagentTimers[msg.id]) clearTimeout(st.subagentTimers[msg.id]);
    st.subagentTimers[msg.id] = setTimeout(() => {
      const bot = chatStore.getChat(sid)?.messages.find(m => m.id === botIdNow);
      const s = bot?.subagents?.find(sa => sa.id === msg.id);
      if (s && s.status === 'running') {
        chatStore.updateSubagent(sid, botIdNow, msg.id, {
          status: 'error',
          error: 'Timed out',
          endTime: Date.now(),
        });
        if (bot) checkAndFinalizeSubagents(bot, turnState(sid));
      }
      const timers = turnState(sid).subagentTimers;
      delete timers[msg.id];
    }, SUBAGENT_TIMEOUT_MS);
  };

  const handleSubagentThinking = (msg: Extract<WsMessage, { type: 'subagent_thinking' }>, botMsg: Message, sid: string) => {
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
      chatStore.updateSubagent(sid, botMsg.id, msg.id, { timeline });
    }
  };

  const handleSubagentContent = (msg: Extract<WsMessage, { type: 'subagent_content' }>, botMsg: Message, sid: string) => {
    const s = getBotSubagent(botMsg, msg.id);
    if (s) {
      chatStore.updateSubagent(sid, botMsg.id, msg.id, { content: (s.content || '') + msg.content });
    }
  };

  const handleSubagentToolStart = (msg: Extract<WsMessage, { type: 'subagent_tool_start' }>, botMsg: Message, sid: string) => {
    const s = getBotSubagent(botMsg, msg.id);
    if (s) {
      // The subagent wire protocol has no tool_call_id yet — event_map's
      // subagent_tool_start carries only the subagent id, name and arguments
      // (unlike the main agent's tool_start, which does serialize one). The
      // client therefore mints the id and pairs the matching tool_end by
      // name; see handleSubagentToolEnd.
      const toolCall = {
        id: makeId(),
        name: msg.name,
        arguments: msg.arguments,
        status: 'running' as const,
        output: null,
        startTime: Date.now(),
      };
      chatStore.updateSubagent(sid, botMsg.id, msg.id, {
        toolCalls: [...s.toolCalls, toolCall],
        toolCount: s.toolCount + 1,
        timeline: [...s.timeline, { kind: 'tool', toolId: toolCall.id }],
      });
    }
  };

  const handleSubagentToolEnd = (msg: Extract<WsMessage, { type: 'subagent_tool_end' }>, botMsg: Message, sid: string) => {
    const s = getBotSubagent(botMsg, msg.id);
    if (s && s.toolCalls.length > 0) {
      // Sub-agents execute tools serially, so the newest running call with
      // this name is the one that just finished. Pairing stays name-based
      // by protocol necessity: subagent_tool_end carries no tool_call_id
      // (only subagent id, name, output) — see subagent_event_to_ws in
      // conga-host. Until the host adds the id, exact-id matching is not
      // possible here.
      const target = [...s.toolCalls].reverse().find(t => t.name === msg.name && t.status === 'running');
      if (target) {
        const elapsed = target.startTime ? Date.now() - target.startTime : 0;
        const newTools = s.toolCalls.map(t =>
          t === target
            ? { ...t, status: 'complete' as const, output: msg.output || null, duration: (elapsed / 1000).toFixed(1) + 's' }
            : t
        );
        chatStore.updateSubagent(sid, botMsg.id, msg.id, { toolCalls: newTools });
      }
    }
  };

  const checkAndFinalizeSubagents = (botMsg: Message, st: TurnState) => {
    const subs = botMsg.subagents;
    if (subs && subs.length > 0 && subs.every(s => s.status !== 'running')) {
      st.subagentPhase = 'completed';
    }
  };

  const handleSubagentCompleted = (msg: Extract<WsMessage, { type: 'subagent_completed' }>, botMsg: Message, sid: string, st: TurnState) => {
    if (st.subagentTimers[msg.id]) {
      clearTimeout(st.subagentTimers[msg.id]);
      delete st.subagentTimers[msg.id];
    }
    chatStore.updateSubagent(sid, botMsg.id, msg.id, {
      status: 'completed',
      summary: msg.summary,
      toolCount: msg.tool_count,
      endTime: Date.now(),
    });
    checkAndFinalizeSubagents(botMsg, st);
  };

  const handleSubagentError = (msg: Extract<WsMessage, { type: 'subagent_error' }>, botMsg: Message, sid: string, st: TurnState) => {
    if (st.subagentTimers[msg.id]) {
      clearTimeout(st.subagentTimers[msg.id]);
      delete st.subagentTimers[msg.id];
    }
    chatStore.updateSubagent(sid, botMsg.id, msg.id, {
      status: 'error',
      error: msg.error,
      endTime: Date.now(),
    });
    checkAndFinalizeSubagents(botMsg, st);
  };

  // ── WebSocket message processing ────────────────────────────

  const processWebSocketMessageInner = (msg: WsMessage, botMsg: Message, sid: string, st: TurnState) => {
    // Non-delta events flush buffered deltas first: they observe the message
    // (done reads content, tools order against thinking timestamps) and must
    // not overtake text that already arrived.
    if (msg.type !== 'thinking' && msg.type !== 'content' && msg.type !== 'text') {
      flushDeltas();
    }
    switch (msg.type) {
      case 'thinking':
        st.isThinking = true;
        queueDelta(sid, botMsg.id, msg.content, 'thinking');
        break;
      case 'tool_start':
        st.isThinking = true;
        chatStore.ensureToolCalls(sid, botMsg.id);
        const toolId = msg.tool_call_id || makeId();
        chatStore.pushToolCall(sid, botMsg.id, {
          id: toolId,
          name: msg.name,
          arguments: msg.arguments || '',
          status: 'running',
          result: null,
          startTime: Date.now()
        });
        break;
      case 'tool_end':
        st.isThinking = true;
        const toolCalls = chatStore.getChat(sid)?.messages.find(m => m.id === botMsg.id)?.toolCalls;
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
          chatStore.updateToolCall(sid, botMsg.id, runningTool.id, updates);
        } else {
          chatStore.ensureToolCalls(sid, botMsg.id);
          chatStore.pushToolCall(sid, botMsg.id, {
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
        st.isThinking = false;
        queueDelta(sid, botMsg.id, msg.content, 'content');
        break;
      case 'error':
        st.isThinking = false;
        st.isReceiving = false;
        st.subagentPhase = 'idle';
        chatStore.abortSubagents(sid, botMsg.id);
        Object.values(st.subagentTimers).forEach(clearTimeout);
        st.subagentTimers = {};
        showError(msg.content || msg.message || 'An error occurred');
        break;
      case 'done':
        // Turn complete: release the tracked bot message.
        st.currentBotMessageId = null;
        st.isThinking = false;
        // 回合结束（含审批超时/连接关闭后的 done）：清理残留审批弹窗。
        // 网关保证 done 排在全部 subagent 事件之后（单一有序通道），
        // 到达这里时子面板必然已收尾。
        st.pendingApprovals.clear();
        st.isReceiving = false;
        // Turn summary: `done_with_summary` carries cumulative tokens + elapsed.
        // Absent for slash-command replies and pre-summary turns.
        if (msg.usage_in != null && msg.usage_out != null && msg.elapsed_ms != null) {
          chatStore.setTurnSummary(sid, botMsg.id, {
            usageIn: msg.usage_in,
            usageOut: msg.usage_out,
            // Cache totals are optional on the wire (older hosts omit them);
            // undefined keeps "absent = unknown" instead of a false 0.
            cacheRead: msg.usage_cache_read ?? undefined,
            cacheWrite: msg.usage_cache_write ?? undefined,
            elapsedMs: msg.elapsed_ms,
          });
        }
        fetchContext(sid);
        // Notify only for replies with actual content — a slash-command
        // echo ("(session cleared)") is not worth a system notification.
        if (botMsg.content.trim()) {
          notifyTurnComplete(
            chatStore.getChat(sid)?.name || 'Conga',
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
        st.isReceiving = false;
        showError(msg.message || 'The agent is busy processing a request');
        break;
      case 'queued': {
        // 中途消息已入列（steer）：渲染为待处理用户气泡。循环会在下一
        // 次 LLM 调用前把它作为真实 User 消息注入并落盘。
        st.isReceiving = false;
        chatStore.appendMessage(sid, {
          id: `queued-${Date.now()}`,
          role: 'user',
          content: msg.message,
          timestamp: Date.now(),
          pending: true,
        });
        break;
      }
      case 'subagent_started':
        handleSubagentStarted(msg, botMsg, sid, st);
        break;
      case 'subagent_all_started':
        break;
      case 'subagent_thinking':
        handleSubagentThinking(msg, botMsg, sid);
        break;
      case 'subagent_content':
        handleSubagentContent(msg, botMsg, sid);
        break;
      case 'subagent_tool_start':
        handleSubagentToolStart(msg, botMsg, sid);
        break;
      case 'subagent_tool_end':
        handleSubagentToolEnd(msg, botMsg, sid);
        break;
      case 'subagent_completed':
        handleSubagentCompleted(msg, botMsg, sid, st);
        break;
      case 'subagent_error':
        handleSubagentError(msg, botMsg, sid, st);
        break;
      case 'subagent_synthesizing':
        st.subagentPhase = 'synthesizing';
        setTimeout(() => { st.subagentPhase = 'completed' }, 300);
        break;
      case 'approval_request':
        st.pendingApprovals.set(msg.id, {
          id: msg.id,
          tool_name: msg.tool_name,
          description: msg.description,
          arguments: msg.arguments,
          preview: msg.preview,
        });
        break;
    }
  };

  const processWebSocketMessage = (raw: unknown, sid: string) => {
    const parsed = parseWsMessage(raw);
    if (!parsed) return; // unknown/absent discriminant — drop, nothing sane to do

    const st = turnState(sid);
    st.isSending = false;
    st.isReceiving = true;

    let botMsg: Message | null = null;
    if (st.currentBotMessageId) {
      const tracked = chatStore.getChat(sid)?.messages.find(m => m.id === st.currentBotMessageId);
      if (tracked) botMsg = tracked;
    }
    if (!botMsg) {
      botMsg = chatStore.getOrCreateBotMessage(sid);
      if (botMsg) st.currentBotMessageId = botMsg.id;
    }
    if (!botMsg) return;
    processWebSocketMessageInner(parsed, botMsg, sid, st);
  };

  const handleMessage = (data: string, sid: string) => {
    try {
      const msg = JSON.parse(data);
      processWebSocketMessage(msg, sid);
    } catch (e) {
      const st = turnState(sid);
      st.isThinking = false;
      st.isSending = false;
      console.error('Malformed gateway frame:', e, data.slice(0, 200));
      showError('Received a malformed message from the server');
    }
  };

  // Transport selection: inside the desktop shell the chat runs in-process
  // over Tauri IPC; in a plain browser (dev) it keeps using the gateway's
  // WebSocket. Both transports deliver the same wire events to handleMessage,
  // each tagged with the session that owns them.
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

  const fetchContext = async (sid: string = chatId.value) => {
    try {
      if (isTauri) {
        // Tauri: invoke the in-process get_context command (mirrors the
        // gateway's GET /api/sessions/:id/context). Same
        // { context_stats } JSON shape.
        const data = await invoke<{ context_stats?: ContextStats }>('get_context', { sessionId: sid });
        if (data?.context_stats) {
          chatStore.setContextStats(sid, data.context_stats);
        }
        return;
      }
      const res = await gatewayFetch(`/api/sessions/${encodeURIComponent(sid)}/context`);
      const data = await res.json();
      if (res.ok && data.context_stats) {
        chatStore.setContextStats(sid, data.context_stats);
      }
    } catch (e) {
      console.error('Fetch context failed:', e);
    }
  };

  // Hydrate the transcript from the backend's authoritative store.
  const fetchMessages = async () => {
    // Hydrating over a live stream would clobber in-flight state — skip.
    if (isTurnBusy(chatId.value)) return;
    const targetId = chatId.value;
    const messages = await fetchSessionMessages(targetId);
    if (!messages) return; // 404 for local-only chats, or request failed
    if (chatId.value !== targetId) return; // user switched chats mid-fetch
    if (isTurnBusy(targetId)) return; // a turn started while fetching
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

  /** Reset a session's latched turn state and mark its still-running store
   * entities (tool calls, subagents) as cancelled. Used when the turn is
   * known dead without a terminal event: cancel, socket loss, or — in
   * browser mode — switching away (the per-session socket closes and the
   * gateway cancels the abandoned turn server-side). */
  const abortTurnState = (sid: string) => {
    const st = turnState(sid);
    st.isThinking = false;
    st.isReceiving = false;
    st.isSending = false;
    st.pendingApprovals.clear();
    chatStore.abortToolCalls(sid);
    // Clear sub-agent state: cancel aborts sub-agent tasks, so Synthesizing
    // never arrives — without this the panels would spin until the 5-minute
    // client timeout.
    st.subagentPhase = 'idle';
    // Read-only lookup: getOrCreate would fabricate an empty bot bubble when
    // there is no reply in flight.
    const lastBotMsg = [...(chatStore.getChat(sid)?.messages || [])].reverse().find(m => m.role === 'bot');
    if (lastBotMsg) chatStore.abortSubagents(sid, lastBotMsg.id);
    Object.values(st.subagentTimers).forEach(clearTimeout);
    st.subagentTimers = {};
  };

  // Auto-fetch context and transcript when connection is established or restored
  watch(isConnected, (connected, prev) => {
    if (connected && !prev) {
      fetchContext();
      fetchMessages();
      return;
    }
    if (!connected) {
      // The gateway cancels a turn whose socket dropped mid-stream, so its
      // terminal event never arrives; without this reset the active chat's
      // input would stay locked until reload. Reconnect rehydrates the
      // transcript from the event log.
      abortTurnState(chatId.value);
    }
  });

  // Hydrate on chat switch and on mount. In browser mode the switch tears
  // down the old session's socket (the gateway then cancels its turn), so
  // reset that session's latched flags; in Tauri mode the broadcast channel
  // keeps streaming into the old session and its state stays live.
  // Only fetch when already connected: a fresh connect flips isConnected
  // false→true right after, and the watcher above is the single hydration
  // point for that edge — fetching here too would double the transcript
  // request on every startup and browser-mode switch.
  watch(() => chatId.value, (_newId, oldId) => {
    if (oldId && !isTauri) abortTurnState(oldId);
    if (isConnected.value) fetchMessages();
  }, { immediate: true });

  const forceCompact = async () => {
    if (isCompacting.value) return;
    isCompacting.value = true;
    try {
      // Tauri has no compaction endpoint; refreshing context is enough.
      if (isTauri) {
        await fetchContext();
        return;
      }
      const res = await gatewayFetch(`/api/sessions/${encodeURIComponent(chatId.value)}/context/compact`, { method: 'POST' });
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
    // Route the cancel to the session that OWNS the running turn — normally
    // the active chat, but a background turn (Tauri broadcast) must not be
    // cancelled under the active chat's id.
    const target = isTurnBusy(chatId.value)
      ? chatId.value
      : ([...turnStates.keys()].find(id => isTurnBusy(id)) ?? chatId.value);
    send(JSON.stringify({ type: 'cancel' }), target);
    abortTurnState(target);
  };

  const sendApprovalResponse = (requestId: string, approved: boolean, remember: boolean = false) => {
    send(JSON.stringify({
      type: 'approval_response',
      request_id: requestId,
      approved,
      remember,
    }));
    turnState(chatId.value).pendingApprovals.delete(requestId);
  };


  const sendMessage = (text: string) => {
    const st = turnState(chatId.value);
    // 接收期间一律禁发（含子 agent 运行中）：后端在回合内不会接受新
    // message，发送只会被静默丢弃——之前允许 running 期间发送是个
    // 契约错觉。
    if (!text.trim() || !isConnected.value || st.isSending || st.isReceiving) return false;

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
    st.currentBotMessageId = null;

    st.isSending = true;
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
    st.isSending = false;
    chatStore.updateMessageStatus(chatId.value, msgId, 'error');
    return false;
  };

  const retryMessage = (msgId: string, content: string) => {
    if (!isConnected.value) return;
    const st = turnState(chatId.value);
    chatStore.updateMessageStatus(chatId.value, msgId, 'sending');
    // Retried turn gets its own bot message; do not append to a stale one.
    st.currentBotMessageId = null;
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

  onUnmounted(() => {
    // The batcher and the safety-net subagent timers must not outlive the
    // component: cancel the pending frame, flush what is buffered (those
    // deltas are real transcript content), and stop every timer.
    if (deltaRafId !== null) cancelAnimationFrame(deltaRafId);
    flushDeltas();
    if (errorBannerTimer) clearTimeout(errorBannerTimer);
    for (const st of turnStates.values()) {
      Object.values(st.subagentTimers).forEach(clearTimeout);
      st.subagentTimers = {};
    }
  });

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
