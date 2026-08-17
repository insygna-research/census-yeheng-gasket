import { invoke } from '@tauri-apps/api/core';
import { isTauri } from '@/lib/platform';
import { readString, storageKeys } from '@/lib/storage';
import type { Message, ToolCall, EnvSettingsView, EnvSettingsPayload } from '@/types';

/**
 * Session storage access layer.
 * The backend's on-disk store (`~/.conga/sessions`, owned by the Rust
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

/** The stored gateway token (empty = none entered yet). Browser mode only;
 * the desktop shell talks IPC and never needs it. */
export function gatewayToken(): string {
  return readString(storageKeys.gatewayToken, '');
}

/** fetch() against the gateway with the auth token attached (Bearer header).
 * Every browser-mode REST call must go through this — the gateway rejects
 * unauthenticated /api/* requests with 401. */
export function gatewayFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers || {});
  const token = gatewayToken();
  if (token && !headers.has('authorization')) {
    headers.set('authorization', `Bearer ${token}`);
  }
  return fetch(`${backendBaseUrl()}${path}`, { ...init, headers });
}

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
  const res = await gatewayFetch(`/api/sessions`);
  if (!res.ok) return [];
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
    const res = await gatewayFetch(`/api/sessions/${encodeURIComponent(chatId)}/name`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

// ── LLM env settings (settings.json on the backend) ──────────────────────

/** Masked settings view (raw keys never cross this boundary). */
export async function fetchEnvSettings(): Promise<EnvSettingsView | null> {
  try {
    if (isTauri) {
      return await invoke<EnvSettingsView>('get_env_settings');
    }
    const res = await gatewayFetch(`/api/settings`);
    if (!res.ok) return null;
    return (await res.json()) as EnvSettingsView;
  } catch {
    return null;
  }
}

/**
 * Persist settings. A group's blank `apiKey` keeps the stored key; a
 * `null` group clears it (env config applies again). The next LLM call
 * uses the new provider — the backend re-resolves it every turn.
 * Returns the updated masked view, or null on validation/transport error
 * (callers surface the error text via the dialog).
 */
export async function saveEnvSettings(
  payload: EnvSettingsPayload
): Promise<{ view: EnvSettingsView } | { error: string }> {
  try {
    if (isTauri) {
      const view = await invoke<EnvSettingsView>('set_env_settings', { payload });
      return { view };
    }
    const res = await gatewayFetch(`/api/settings`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(payload),
    });
    if (!res.ok) {
      const body = (await res.json().catch(() => null)) as { error?: string } | null;
      return { error: body?.error || `save failed (HTTP ${res.status})` };
    }
    return { view: (await res.json()) as EnvSettingsView };
  } catch (e) {
    return { error: String(e) };
  }
}

/** Delete the session's on-disk data. False when the request failed. */
export async function deleteSession(chatId: string): Promise<boolean> {
  try {
    if (isTauri) {
      return await invoke<boolean>('delete_session', { id: chatId });
    }
    const res = await gatewayFetch(`/api/sessions/${encodeURIComponent(chatId)}`, {
      method: 'DELETE',
    });
    return res.ok;
  } catch {
    return false;
  }
}

// ── Message mapping ─────────────────────────────────────────
// Gateway shape (conga AgentMessage, serde tag = "role"):
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
    const res = await gatewayFetch(`/api/sessions/${encodeURIComponent(chatId)}/messages`);
    if (!res.ok) return null;
    const list = await res.json();
    if (!Array.isArray(list) || list.length === 0) return null;
    return mapBackendMessages(list);
  } catch {
    return null;
  }
}

/** Cross-session full-text search hit — the gateway REST route and the
 * desktop Tauri command return the same shape (single engine, single
 * SessionHit definition in conga-host). */
export interface SessionHit {
  session_id: string;
  /** Display name from the session's meta sidecar; null when unnamed. */
  name?: string | null;
  snippet: string;
}

/** Cross-session full-text search. Empty list on failure or no hits —
 * callers render an empty result, not an error toast. */
export async function searchSessions(q: string): Promise<SessionHit[]> {
  try {
    if (isTauri) {
      return await invoke<SessionHit[]>('search_sessions', { query: q });
    }
    const res = await gatewayFetch(`/api/sessions/search?q=${encodeURIComponent(q)}`);
    if (!res.ok) return [];
    const data = await res.json();
    return data.hits || [];
  } catch {
    return [];
  }
}
