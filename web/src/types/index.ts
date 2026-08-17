// Subagent message types for real-time UI updates

/**
 * Tool call state for subagent execution
 */
export interface SubagentToolCall {
  id: string;
  name: string;
  arguments?: string;
  status: 'running' | 'complete' | 'error';
  output?: string | null;
  duration?: string;
  /** Start timestamp (ms) — duration is derived from this, never parsed from id. */
  startTime?: number;
}

/**
 * One ordered entry in a subagent's run. Thinking blocks and tool calls are
 * recorded in arrival order (thinking → tool → thinking → tool …), so the UI
 * can render the actual sequence instead of grouping by kind.
 * Tool entries reference SubagentToolCall.id — the call itself lives in
 * SubagentState.toolCalls and is resolved at render time.
 */
export type SubagentTimelineEntry =
  | { kind: 'thinking'; text: string }
  | { kind: 'tool'; toolId: string };

/**
 * Subagent execution state
 * Used for real-time tracking of parallel subagent tasks
 */
export interface SubagentState {
  /** Unique subagent ID (UUID) */
  id: string;
  /** Task index (1-indexed for display) */
  index: number;
  /** Task description */
  task: string;
  /** Execution status */
  status: 'running' | 'completed' | 'error';
  /** Ordered run timeline: thinking blocks and tool calls interleaved in
   * arrival order. Thinking/reasoning text lives here (consecutive chunks
   * merge into one block); there is no separate aggregated thinking field. */
  timeline: SubagentTimelineEntry[];
  /** Incremental output content */
  content?: string;
  /** Tool calls made by this subagent */
  toolCalls: SubagentToolCall[];
  /** Total tool call count */
  toolCount: number;
  /** Brief summary (first 200 chars of the sub-agent's final text) */
  summary?: string;
  /** Error message if status is 'error' */
  error?: string;
  /** Start timestamp (ms) */
  startTime: number;
  /** End timestamp (ms) if completed */
  endTime?: number;
}

/**
 * The gateway→frontend wire protocol, field-for-field.
 * Source of truth: `conga-host/src/wire.rs` (OutgoingEvent) and
 * `conga-host/src/event_map.rs::subagent_event_to_ws`. Add fields, never
 * rename — this union IS the contract.
 */
export type WsMessage =
  // ── main agent stream ──
  | { type: 'thinking'; content: string }
  | { type: 'content'; content: string }
  | { type: 'text'; content: string } // legacy alias still accepted
  | { type: 'tool_start'; name: string; arguments?: string; tool_call_id?: string }
  | { type: 'tool_end'; name: string; output?: string; error?: string; tool_call_id?: string }
  | { type: 'error'; content?: string; message?: string }
  | { type: 'done'; usage_in?: number; usage_out?: number; elapsed_ms?: number }
  | { type: 'busy'; content?: string; message?: string }
  | { type: 'queued'; message: string }
  | { type: 'approval_request'; id: string; tool_name: string; description: string; arguments: string; preview?: string }
  // ── subagent_* (all 9 wire variants; Usage never leaves the server) ──
  | SubagentWsMessage;

/**
 * Runtime guard: a parsed WS/Tauri payload with a known `type` discriminant.
 * Unknown types (future protocol) fall through as `null`.
 */
const WS_MESSAGE_TYPES = new Set([
  'thinking', 'content', 'text', 'tool_start', 'tool_end', 'error', 'done',
  'busy', 'queued', 'approval_request',
  'subagent_all_started', 'subagent_synthesizing', 'subagent_started',
  'subagent_thinking', 'subagent_content', 'subagent_tool_start',
  'subagent_tool_end', 'subagent_completed', 'subagent_error',
]);

export function parseWsMessage(raw: unknown): WsMessage | null {
  if (typeof raw !== 'object' || raw === null || !('type' in raw)) {
    return null;
  }
  const t = raw.type; // narrowed to unknown by the `in` check above
  if (typeof t === 'string' && WS_MESSAGE_TYPES.has(t)) {
    // Known discriminant: this is a WsMessage by construction of the
    // server-side serializer (wire.rs / event_map.rs).
    const msg: WsMessage = raw as WsMessage;
    return msg;
  }
  return null;
}

/**
 * Subagent message types for real-time UI updates
 */
export type SubagentWsMessage =
  | { type: 'subagent_all_started'; count: number }
  | { type: 'subagent_synthesizing' }
  | { type: 'subagent_started'; id: string; task: string; index: number }
  | { type: 'subagent_thinking'; id: string; content: string }
  | { type: 'subagent_content'; id: string; content: string }
  | { type: 'subagent_tool_start'; id: string; name: string; arguments?: string }
  | { type: 'subagent_tool_end'; id: string; name: string; output?: string }
  | { type: 'subagent_completed'; id: string; index: number; summary: string; tool_count: number }
  | { type: 'subagent_error'; id: string; index: number; error: string };

/**
 * Type guard to check if a WebSocket message is a subagent message
 */
export function isSubagentMessage(msg: { type: string }): msg is SubagentWsMessage {
  return msg.type.startsWith('subagent_');
}

// ── IM Types ────────────────────────────────────────────────

export interface ToolCall {
  id: string;
  name: string;
  arguments?: string;
  status: 'running' | 'complete' | 'error';
  result?: string | null;
  duration?: string;
  startTime?: number;
}

export interface ThinkingChunk {
  content: string;
  timestamp: number;
}

export type TimelineItem =
  | { type: 'thinking'; content: string; timestamp: number }
  | { type: 'tool_call'; tool: ToolCall; timestamp: number };

export type MessageStatus = 'sending' | 'sent' | 'error';

export interface Message {
  id: string;
  role: 'user' | 'bot' | 'system';
  content: string;
  thinking?: string;
  thinkingChunks?: ThinkingChunk[];
  toolCalls?: ToolCall[];
  /** Subagent states attached to this message for persistent display */
  subagents?: SubagentState[];
  timestamp: number;
  status?: MessageStatus;
  pending?: boolean;
  /** Turn summary: cumulative tokens + elapsed time, populated when the
   * `done` event for this bot reply carries `usage_in`/`usage_out`/`elapsed_ms`.
   * Absent for slash-command replies and pre-summary turns. */
  turnSummary?: TurnSummary;
}

/** Usage line shown after a completed turn. */
export interface TurnSummary {
  /** Cumulative input tokens across the session so far. */
  usageIn: number;
  /** Cumulative output tokens across the session so far. */
  usageOut: number;
  /** Wall-clock duration of this turn in milliseconds. */
  elapsedMs: number;
}

/** Mirrors the JSON shape sent by both the gateway's
 * `GET /api/sessions/:id/context` and the Tauri `get_context` command. */
export interface ContextStats {
  current_tokens: number;
  usage_percent: number;
  is_compressing: boolean;
  cumulative_in: number;
  cumulative_out: number;
}

export interface Chat {
  id: string;
  name: string;
  messages: Message[];
  updatedAt: number;
  contextStats?: ContextStats;
}

// ── Approval Types ──────────────────────────────────────────

export interface ApprovalRequest {
  id: string;
  tool_name: string;
  description: string;
  arguments?: string;
  /** Human-readable diff preview for file-mutating tools (edit/write). */
  preview?: string;
}

export interface ApprovalResponse {
  request_id: string;
  approved: boolean;
  remember?: boolean;
}
