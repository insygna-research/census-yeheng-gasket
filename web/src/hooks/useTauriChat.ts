import { onUnmounted, ref, type Ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

/**
 * Tauri in-process chat transport.
 *
 * Inside the desktop shell there is no gateway/WebSocket: the Rust backend
 * (src-tauri chat.rs) assembles one Host per session and streams turn events
 * over IPC. This composable exposes the same surface as useIMWebSocket so
 * useChatSession can swap transports with a single isTauri branch.
 *
 * The `chat-event` payload carries the gateway's wire event unchanged (plus a
 * session id for routing), so the shared message handler parses it exactly
 * like a WebSocket frame.
 */

interface ChatEventPayload {
  session_id: string;
  /** One gateway-protocol event (thinking, content, tool_*, done, error, busy, approval_request, subagent_*). */
  event: unknown;
}

export function useTauriChat(
  chatId: Ref<string>,
  onMessage: (data: string) => void
) {
  // The in-process channel has no connection lifecycle: it is "connected"
  // as soon as the listener is installed, and there is nothing to reconnect.
  const isConnected = ref(false);
  const showReconnectButton = ref(false);
  let unlisten: UnlistenFn | null = null;

  const connect = () => {
    if (unlisten) return;
    listen<ChatEventPayload>('chat-event', e => {
      // One broadcast channel carries every session; route by id. The
      // closure reads chatId lazily, so chat switches need no re-subscribe.
      if (e.payload.session_id !== chatId.value) return;
      onMessage(JSON.stringify(e.payload.event));
    })
      .then(u => {
        unlisten = u;
        isConnected.value = true;
      })
      .catch(e => console.error('chat-event listen failed:', e));
  };

  /**
   * Accepts the same WS-shaped JSON strings the WebSocket transport sends
   * and dispatches them to the matching Tauri command.
   */
  const send = (data: string): boolean => {
    let msg: {
      type?: string;
      content?: string;
      trace_id?: string;
      request_id?: string;
      approved?: boolean;
      remember?: boolean;
    };
    try {
      msg = JSON.parse(data);
    } catch {
      return false;
    }
    // Tauri v2 maps camelCase invoke args onto the commands' snake_case params.
    const cmd =
      msg.type === 'message'
        ? invoke('send_message', {
            sessionId: chatId.value,
            content: msg.content ?? '',
            traceId: msg.trace_id ?? null,
          })
        : msg.type === 'cancel'
          ? invoke('cancel_turn', { sessionId: chatId.value })
          : msg.type === 'approval_response'
            ? invoke('approval_response', {
                sessionId: chatId.value,
                requestId: msg.request_id,
                approved: !!msg.approved,
                remember: !!msg.remember,
              })
            : null;
    if (!cmd) return false;
    cmd.catch(e => console.error(`chat invoke ${msg.type} failed:`, e));
    return true;
  };

  // No socket to reconnect; kept only for interface parity with useIMWebSocket.
  const manualReconnect = () => {
    if (!unlisten) connect();
  };

  onUnmounted(() => {
    unlisten?.();
    unlisten = null;
  });

  return {
    isConnected,
    showReconnectButton,
    connect,
    manualReconnect,
    send,
  };
}
