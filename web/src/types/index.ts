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
}

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
  /** Incremental thinking/reasoning content */
  thinking?: string;
  /** Incremental output content */
  content?: string;
  /** Tool calls made by this subagent */
  toolCalls: SubagentToolCall[];
  /** Total tool call count */
  toolCount: number;
  /** Brief summary (first 100 chars of result) */
  summary?: string;
  /** Error message if status is 'error' */
  error?: string;
  /** Start timestamp (ms) */
  startTime: number;
  /** End timestamp (ms) if completed */
  endTime?: number;
}

/**
 * WebSocket message types for subagent events.
 * M2 规划：网关当前不发送这些消息（core 子 agent 编排未实现），
 * 类型保留供 M2 使用，勿删除。
 */
export type SubagentWsMessage =
  | { type: 'subagent_all_started'; count: number }
  | { type: 'subagent_synthesizing' }
  | { type: 'subagent_started'; id: string; task: string; index: number }
  | { type: 'subagent_thinking'; id: string; content: string }
  | { type: 'subagent_content'; id: string; content: string }
  | { type: 'subagent_tool_start'; id: string; name: string; arguments?: string }
  | { type: 'subagent_tool_end'; id: string; tool_id?: string; name: string; output?: string }
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
}

export interface ContextStats {
  token_budget: number;
  compaction_threshold: number;
  threshold_tokens: number;
  current_tokens: number;
  usage_percent: number;
  is_compressing: boolean;
}

export interface WatermarkInfo {
  watermark: number;
  max_sequence: number;
  uncompacted_count: number;
  compacted_percent: number;
}

export interface Chat {
  id: string;
  name: string;
  messages: Message[];
  updatedAt: number;
  contextStats?: ContextStats;
  watermarkInfo?: WatermarkInfo;
}

// ── Approval Types ──────────────────────────────────────────

export interface ApprovalRequest {
  id: string;
  tool_name: string;
  description: string;
  arguments?: string;
}

export interface ApprovalResponse {
  request_id: string;
  approved: boolean;
  remember?: boolean;
}
