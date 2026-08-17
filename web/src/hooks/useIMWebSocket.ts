import { readString, storageKeys } from '@/lib/storage';
import { onUnmounted, ref, watch, type Ref } from 'vue';


export function useIMWebSocket(
  chatId: Ref<string>,
  onMessage: (data: string, sessionId: string) => void
) {
  const ws = ref<WebSocket | null>(null);
  const isConnected = ref(false);
  const isReconnecting = ref(false);
  const showReconnectButton = ref(false);
  const reconnectAttempts = ref(0);

  const maxReconnectAttempts = 5;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

  const connect = () => {
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }

    if (ws.value) {
      // Detach the old socket's handlers before closing it. Its onclose fires
      // asynchronously — after this function returns. With a local (Tauri)
      // gateway the new socket's onopen runs first and resets the reconnect
      // state, so the stale socket's later onclose would spuriously kick off
      // attemptReconnect, forming a self-sustaining reconnect loop. Nulling
      // the handlers makes the replaced socket's close silent.
      ws.value.onopen = null;
      ws.value.onclose = null;
      ws.value.onerror = null;
      ws.value.close();
    }
    isConnected.value = false;

    const token = readString(storageKeys.gatewayToken, '');
    const wsUrl = `${import.meta.env.VITE_WS_URL || 'ws://localhost:3000'}/ws?user_id=${encodeURIComponent(chatId.value)}${token ? `&token=${encodeURIComponent(token)}` : ''}`;
    ws.value = new WebSocket(wsUrl);

    ws.value.onopen = () => {
      isConnected.value = true;
      reconnectAttempts.value = 0;
      showReconnectButton.value = false;
      isReconnecting.value = false;
    };

    ws.value.onmessage = (event) => {
      // The socket is bound to the session it connected with (user_id
      // query param), so every frame on it belongs to chatId.value.
      onMessage(event.data, chatId.value);
    };

    ws.value.onclose = () => {
      isConnected.value = false;
      attemptReconnect();
    };

    ws.value.onerror = () => {
      isConnected.value = false;
    };
  };

  const attemptReconnect = () => {
    if (reconnectAttempts.value >= maxReconnectAttempts) {
      showReconnectButton.value = true;
      isReconnecting.value = false;
      return;
    }

    isReconnecting.value = true;
    const delay = Math.min(1000 * Math.pow(2, reconnectAttempts.value), 30000);
    reconnectAttempts.value++;

    reconnectTimer = setTimeout(() => {
      connect();
    }, delay);
  };

  const manualReconnect = () => {
    reconnectAttempts.value = 0;
    showReconnectButton.value = false;
    isReconnecting.value = true;
    connect();
  };

  // `targetSessionId` is accepted for interface parity with useTauriChat but
  // cannot be honored here: the socket is bound to the session it connected
  // with, so a cross-session cancel cannot ride it. (In browser mode
  // switching chats closes the old socket and the gateway cancels that
  // turn server-side, so no background turn ever needs targeting.)
  const send = (data: string, _targetSessionId?: string): boolean => {
    if (ws.value?.readyState === WebSocket.OPEN) {
      ws.value.send(data);
      return true;
    }
    return false;
  };

  const close = () => {
    if (ws.value) {
      ws.value.onclose = null;
      ws.value.close();
    }
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
  };

  watch(chatId, () => {
    connect();
  });

  onUnmounted(() => {
    close();
  });

  return {
    ws,
    isConnected,
    isReconnecting,
    showReconnectButton,
    reconnectAttempts,
    connect,
    manualReconnect,
    send,
    close
  };
}
