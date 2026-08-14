import { invoke } from '@tauri-apps/api/core';
import { isTauri } from '@/lib/platform';
import type { Message, ToolCall } from '@/types';

/**
 * Session storage access layer.
 * The backend's on-disk store (`~/.gasket/sessions`, owned by the Rust
 * side — Tauri commands in-app, gateway HTTP in the browser) is the single
 * source of truth: list, names, transcripts, and deletes all go through
 * it. Nothing session-shaped is persisted client-side.
 *
 * Dual channel: inside the Tauri desktop shell the session API runs as
 * native commands (self-contained app); in a plain browser (dev) it falls
 * back to the gateway's HTTP endpoints.
 */

export const backendBaseUrl = (): string =>
  import.meta.env.VITE_API_URL || 'http://localhost:3000';

/** Session keys on the HTTP wire carry a `websocket:` prefix the gateway strips. */
export const sessionKey = (chatId: string): string =>
  encodeURIComponent(`websocket:${chatId}`);

export interface BackendSessionInfo {
  id: string;
  msg_count: number;
  mtime: number;
  /** Display name from the session's meta sidecar; null when never renamed. */
  name?: string | null;
}

export async function fetchSessionList(): Promise<BackendSessionInfo[]> {
  if (isTauri) {
    return invoke<BackendSessionInfo[]>('list_sessions');
  }
  const res = await fetch(`${backendBaseUrl()}/api/sessions`);
  const data = await res.json();
  return data.sessions || [];
}

/** Persist the display name backend-side (meta.json sidecar). */
export async function renameSession(chatId: string, name: string): Promise<boolean> {
  try {
    if (isTauri) {
      await invoke('rename_session', { id: chatId, name });
      return true;
    }
    const res = await fetch(`${backendBaseUrl()}/api/sessions/${sessionKey(chatId)}/name`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

/** Delete the session's on-disk data. False when the request failed. */
export async function deleteSession(chatId: string): Promise<boolean> {
  try {
    if (isTauri) {
      return await invoke<boolean>('delete_session', { id: chatId });
    }
    const res = await fetch(`${backendBaseUrl()}/api/sessions/${sessionKey(chatId)}`, {
      method: 'DELETE',
    });
    return res.ok;
  } catch {
    return false;
  }
}

// ── Message mapping ─────────────────────────────────────────
// Gateway shape (gasket-core AgentMessage, serde tag = "role"):
//   user:        { role, content: ContentBlock[], timestamp }
//   assistant:   { role, content: ContentBlock[], model, stop_reason, usage, timestamp }
//   tool_result: { role, tool_call_id, tool_name, content, is_error, timestamp }
//   custom:      { role, custom_type, content: Value, timestamp }  (skipped)
// ContentBlock (serde tag = "type"): text{ text } | image{..} |
//   tool_call{ tool_call:{ id, function:{ name, arguments } } } | thinking{ thinking }

interface BackendContentBlock {
  type: string;
  text?: string;
  thinking?: string;
  tool_call?: { id: string; function: { name: string; arguments: string } };
}

interface BackendMessage {
  role: string;
  content?: BackendContentBlock[];
  timestamp?: number;
  tool_call_id?: string;
  is_error?: boolean;
}

const textOf = (blocks?: BackendContentBlock[]): string =>
  (blocks || [])
    .filter(b => b.type === 'text')
    .map(b => b.text || '')
    .join('\n');

/** Map the gateway's flat AgentMessage array to frontend Message[]. */
export function mapBackendMessages(list: BackendMessage[]): Message[] {
  const out: Message[] = [];
  const toolByCallId = new Map<string, ToolCall>();

  for (const m of list) {
    if (m.role === 'user') {
      const text = textOf(m.content);
      if (!text) continue;
      out.push({
        id: `be_${out.length}_${m.timestamp || 0}`,
        role: 'user',
        content: text,
        timestamp: m.timestamp || Date.now(),
      });
    } else if (m.role === 'assistant') {
      const msg: Message = {
        id: `be_${out.length}_${m.timestamp || 0}`,
        role: 'bot',
        content: '',
        timestamp: m.timestamp || Date.now(),
      };
      for (const b of m.content || []) {
        if (b.type === 'text' && b.text) {
          msg.content += (msg.content ? '\n' : '') + b.text;
        } else if (b.type === 'thinking' && b.thinking) {
          msg.thinking = (msg.thinking || '') + b.thinking;
        } else if (b.type === 'tool_call' && b.tool_call) {
          const tc: ToolCall = {
            id: b.tool_call.id,
            name: b.tool_call.function.name,
            arguments: b.tool_call.function.arguments,
            // Resolved by the matching tool_result below; an interrupted
            // turn leaves it 'running', which the thoughts panel shows as-is.
            status: 'running',
            result: null,
          };
          (msg.toolCalls ??= []).push(tc);
          toolByCallId.set(b.tool_call.id, tc);
        }
      }
      out.push(msg);
    } else if (m.role === 'tool_result') {
      const tc = m.tool_call_id ? toolByCallId.get(m.tool_call_id) : undefined;
      if (tc) {
        tc.result = textOf(m.content);
        tc.status = m.is_error ? 'error' : 'complete';
      }
    }
    // role 'custom' carries no chat content — skip
  }

  return out;
}

/**
 * Fetch the persisted transcript for a session.
 * Returns null when the session has no on-disk data (a local-only chat) or
 * the request fails — callers keep their local state in that case.
 */
export async function fetchSessionMessages(chatId: string): Promise<Message[] | null> {
  try {
    if (isTauri) {
      const list = await invoke<unknown[] | null>('get_session_messages', { id: chatId });
      if (!list || list.length === 0) return null;
      return mapBackendMessages(list as Parameters<typeof mapBackendMessages>[0]);
    }
    const res = await fetch(`${backendBaseUrl()}/api/sessions/${sessionKey(chatId)}/messages`);
    if (!res.ok) return null;
    const list = await res.json();
    if (!Array.isArray(list) || list.length === 0) return null;
    return mapBackendMessages(list);
  } catch {
    return null;
  }
}
