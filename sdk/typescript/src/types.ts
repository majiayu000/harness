export type RpcId = number;

export interface RpcErrorPayload {
  code: number;
  message: string;
  data?: unknown;
}

export interface RpcEnvelope<T> {
  jsonrpc: string;
  id?: RpcId | null;
  result?: T;
  error?: RpcErrorPayload;
}

export interface RpcRequestEnvelope {
  jsonrpc: "2.0";
  id: RpcId;
  method: string;
  params: Record<string, unknown>;
}

export interface FetchResponseLike {
  ok: boolean;
  status: number;
  text(): Promise<string>;
}

export type FetchLike = (
  url: string,
  init: {
    method: string;
    headers: Record<string, string>;
    body: string;
  },
) => Promise<FetchResponseLike>;

export interface HarnessOptions {
  baseUrl?: string;
  cwd?: string;
  fetch?: FetchLike;
  apiToken?: string;
  headers?: Record<string, string>;
  requestTimeoutMs?: number;
  defaultPollIntervalMs?: number;
  defaultRunTimeoutMs?: number;
}

export interface StartThreadOptions {
  cwd?: string;
}

export interface TokenUsage {
  input_tokens: number;
  output_tokens: number;
  total_tokens: number;
  cost_usd: number;
}

export interface UserMessageItem {
  type: "user_message";
  content: string;
}

export interface AgentReasoningItem {
  type: "agent_reasoning";
  content: string;
}

export interface ShellCommandItem {
  type: "shell_command";
  command: string;
  exit_code?: number | null;
  stdout: string;
  stderr: string;
}

export interface FileEditItem {
  type: "file_edit";
  path: string;
  before: string;
  after: string;
}

export interface FileReadItem {
  type: "file_read";
  path: string;
  content: string;
}

export interface ToolCallItem {
  type: "tool_call";
  name: string;
  input: unknown;
  output?: unknown;
}

export interface ApprovalRequestItem {
  type: "approval_request";
  id?: string;
  action: string;
  approved?: boolean | null;
}

export interface ErrorItem {
  type: "error";
  code: number;
  message: string;
}

export interface UnknownTurnItem {
  type: string;
  [key: string]: unknown;
}

export type TurnItem =
  | UserMessageItem
  | AgentReasoningItem
  | ShellCommandItem
  | FileEditItem
  | FileReadItem
  | ToolCallItem
  | ApprovalRequestItem
  | ErrorItem
  | UnknownTurnItem;

export type TurnStatus = "running" | "completed" | "cancelled" | "failed";

export interface TurnSnapshot {
  id: string;
  thread_id: string;
  agent_id?: string | null;
  status: TurnStatus;
  items: TurnItem[];
  started_at?: string | null;
  token_usage?: TokenUsage;
  completed_at?: string | null;
}

export type SdkThreadEventMethod =
  | "sdk:turn/started"
  | "sdk:turn/status"
  | "sdk:turn/completed"
  | "sdk:turn/timeout";

interface ThreadEventBase<Method extends SdkThreadEventMethod, Params> {
  method: Method;
  params: Params;
  timestamp: string;
}

interface ThreadStartedEventParams {
  thread_id: string;
  turn_id: string;
  source: "sdk-poll";
  server_method: "turn/start";
}

interface ThreadStatusEventParams {
  thread_id: string;
  turn_id: string;
  turn: TurnSnapshot;
  source: "sdk-poll";
  server_method: "turn/status";
}

interface ThreadCompletedEventParams {
  thread_id: string;
  turn_id: string;
  status: TurnStatus;
  token_usage: TokenUsage | null;
  source: "sdk-poll";
  server_method: "turn/status";
}

interface ThreadTimeoutEventParams {
  thread_id: string;
  turn_id: string;
  timeout_ms: number;
  source: "sdk-poll";
  server_method: "turn/status";
}

export type ThreadStartedEvent = ThreadEventBase<"sdk:turn/started", ThreadStartedEventParams>;
export type ThreadStatusEvent = ThreadEventBase<"sdk:turn/status", ThreadStatusEventParams>;
export type ThreadCompletedEvent = ThreadEventBase<"sdk:turn/completed", ThreadCompletedEventParams>;
export type ThreadTimeoutEvent = ThreadEventBase<"sdk:turn/timeout", ThreadTimeoutEventParams>;

export type ThreadEvent =
  | ThreadStartedEvent
  | ThreadStatusEvent
  | ThreadCompletedEvent
  | ThreadTimeoutEvent;

export interface RunOptions {
  pollIntervalMs?: number;
  timeoutMs?: number;
  onEvent?: (event: ThreadEvent) => void | Promise<void>;
}

export interface RunResult {
  threadId: string;
  turnId: string;
  status: TurnStatus;
  output: string;
  turn?: TurnSnapshot;
  events: ThreadEvent[];
  timedOut: boolean;
}
