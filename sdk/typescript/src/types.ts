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

interface TurnItemBase {
  [key: string]: unknown;
}

export interface UserMessageItem extends TurnItemBase {
  type: "user_message";
  content: string;
}

export interface AgentReasoningItem extends TurnItemBase {
  type: "agent_reasoning";
  content: string;
}

export interface ShellCommandItem extends TurnItemBase {
  type: "shell_command";
  command: string;
  exit_code?: number | null;
  stdout: string;
  stderr: string;
}

export interface FileEditItem extends TurnItemBase {
  type: "file_edit";
  path: string;
  before: string;
  after: string;
}

export interface FileReadItem extends TurnItemBase {
  type: "file_read";
  path: string;
  content: string;
}

export interface ToolCallItem extends TurnItemBase {
  type: "tool_call";
  name: string;
  input: unknown;
  output?: unknown;
}

export interface ApprovalRequestItem extends TurnItemBase {
  type: "approval_request";
  action: string;
  approved?: boolean | null;
}

export interface ErrorItem extends TurnItemBase {
  type: "error";
  code: number;
  message: string;
}

export type TurnItem =
  | UserMessageItem
  | AgentReasoningItem
  | ShellCommandItem
  | FileEditItem
  | FileReadItem
  | ToolCallItem
  | ApprovalRequestItem
  | ErrorItem;

export interface TurnSnapshot {
  id: string;
  thread_id: string;
  status: string;
  items: TurnItem[];
  token_usage?: TokenUsage;
  completed_at?: string | null;
  [key: string]: unknown;
}

export type SdkThreadEventMethod =
  | "sdk:turn/started"
  | "sdk:turn/status"
  | "sdk:turn/completed"
  | "sdk:turn/timeout";

export interface ThreadEvent {
  method: SdkThreadEventMethod;
  params: Record<string, unknown>;
  timestamp: string;
}

export interface RunOptions {
  pollIntervalMs?: number;
  timeoutMs?: number;
  onEvent?: (event: ThreadEvent) => void | Promise<void>;
}

export interface RunResult {
  threadId: string;
  turnId: string;
  status: string;
  output: string;
  turn?: TurnSnapshot;
  events: ThreadEvent[];
  timedOut: boolean;
}
