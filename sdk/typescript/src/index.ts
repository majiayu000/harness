type RpcId = number;

interface RpcErrorPayload {
  code: number;
  message: string;
  data?: unknown;
}

interface RpcEnvelope<T> {
  jsonrpc: string;
  id?: RpcId | null;
  result?: T;
  error?: RpcErrorPayload;
}

interface RpcRequestEnvelope {
  jsonrpc: "2.0";
  id: RpcId;
  method: string;
  params: Record<string, unknown>;
}

interface FetchResponseLike {
  ok: boolean;
  status: number;
  text(): Promise<string>;
}

type FetchLike = (
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

export interface TurnItem {
  type: string;
  content?: string;
  stdout?: string;
  stderr?: string;
  message?: string;
  [key: string]: unknown;
}

export interface TurnSnapshot {
  id: string;
  thread_id: string;
  status: string;
  items: TurnItem[];
  token_usage?: TokenUsage;
  completed_at?: string | null;
  [key: string]: unknown;
}

export interface ThreadEvent {
  method: string;
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

export class HarnessRpcError extends Error {
  readonly code: number;
  readonly data: unknown;

  constructor(payload: RpcErrorPayload) {
    super(payload.message);
    this.name = "HarnessRpcError";
    this.code = payload.code;
    this.data = payload.data;
  }
}

class RpcTransport {
  private readonly endpoint: string;
  private readonly fetchImpl: FetchLike;
  private readonly requestTimeoutMs: number;
  private nextRequestId: RpcId;

  constructor(options: HarnessOptions) {
    this.endpoint = `${normalizeBaseUrl(options.baseUrl)}/rpc`;
    this.fetchImpl = resolveFetch(options.fetch);
    this.requestTimeoutMs = options.requestTimeoutMs ?? 15_000;
    this.nextRequestId = 1;
  }

  async request<T>(method: string, params: Record<string, unknown>): Promise<T> {
    const payload: RpcRequestEnvelope = {
      jsonrpc: "2.0",
      id: this.nextRequestId++,
      method,
      params,
    };

    const responseText = await withTimeout(
      this.fetchImpl(this.endpoint, {
        method: "POST",
        headers: {
          "content-type": "application/json",
        },
        body: JSON.stringify(payload),
      }).then(async (response) => {
        const body = await response.text();
        if (!response.ok) {
          throw new Error(`HTTP ${response.status}: ${body}`);
        }
        return body;
      }),
      this.requestTimeoutMs,
      `RPC request timeout after ${this.requestTimeoutMs}ms for method '${method}'`,
    );

    let parsed: RpcEnvelope<T>;
    try {
      parsed = JSON.parse(responseText) as RpcEnvelope<T>;
    } catch (error) {
      throw new Error(
        `Failed to parse RPC response for '${method}': ${(error as Error).message}`,
      );
    }

    if (parsed.error) {
      throw new HarnessRpcError(parsed.error);
    }

    if (typeof parsed.result === "undefined") {
      throw new Error(`Missing RPC result for '${method}'`);
    }

    return parsed.result;
  }
}

export class Harness {
  private readonly transport: RpcTransport;
  private readonly defaultCwd: string;
  readonly defaultPollIntervalMs: number;
  readonly defaultRunTimeoutMs: number;

  constructor(options: HarnessOptions = {}) {
    this.transport = new RpcTransport(options);
    this.defaultCwd = options.cwd ?? process.cwd();
    this.defaultPollIntervalMs = options.defaultPollIntervalMs ?? 500;
    this.defaultRunTimeoutMs = options.defaultRunTimeoutMs ?? 30_000;
  }

  async startThread(options: StartThreadOptions = {}): Promise<HarnessThread> {
    const result = await this.transport.request<{ thread_id: string }>("thread/start", {
      cwd: options.cwd ?? this.defaultCwd,
    });
    return new HarnessThread(this, result.thread_id);
  }

  async resumeThread(threadId: string): Promise<HarnessThread> {
    await this.transport.request<{ resumed: boolean }>("thread/resume", {
      thread_id: threadId,
    });
    return new HarnessThread(this, threadId);
  }

  thread(threadId: string): HarnessThread {
    return new HarnessThread(this, threadId);
  }

  async startTurn(threadId: string, prompt: string): Promise<{ turn_id: string }> {
    return this.transport.request<{ turn_id: string }>("turn/start", {
      thread_id: threadId,
      input: prompt,
    });
  }

  async turnStatus(turnId: string): Promise<TurnSnapshot> {
    return this.transport.request<TurnSnapshot>("turn/status", { turn_id: turnId });
  }
}

export class HarnessThread {
  readonly id: string;
  private readonly client: Harness;

  constructor(client: Harness, threadId: string) {
    this.client = client;
    this.id = threadId;
  }

  async run(prompt: string, options: RunOptions = {}): Promise<RunResult> {
    const events: ThreadEvent[] = [];
    let turnId = "";
    let latestTurn: TurnSnapshot | undefined;

    for await (const event of this.runStream(prompt, options)) {
      events.push(event);

      if (event.method === "turn/started") {
        turnId = String(event.params.turn_id ?? "");
      }

      if (event.method === "turn/status") {
        latestTurn = event.params.turn as TurnSnapshot;
      }
    }

    if (!latestTurn && turnId) {
      latestTurn = await this.client.turnStatus(turnId);
    }

    return {
      threadId: this.id,
      turnId,
      status: latestTurn?.status ?? "running",
      output: extractOutput(latestTurn),
      turn: latestTurn,
      events,
      timedOut: events.some((event) => event.method === "turn/timeout"),
    };
  }

  async *runStream(
    prompt: string,
    options: RunOptions = {},
  ): AsyncGenerator<ThreadEvent, void, void> {
    const pollIntervalMs = Math.max(
      10,
      options.pollIntervalMs ?? this.client.defaultPollIntervalMs,
    );
    const timeoutMs = Math.max(1, options.timeoutMs ?? this.client.defaultRunTimeoutMs);
    const startedAt = Date.now();
    let previousSignature = "";

    const started = await this.client.startTurn(this.id, prompt);
    const turnId = started.turn_id;

    yield* emitEvent(
      {
        method: "turn/started",
        params: {
          thread_id: this.id,
          turn_id: turnId,
          source: "rpc",
        },
        timestamp: new Date().toISOString(),
      },
      options.onEvent,
    );

    while (true) {
      const turn = await this.client.turnStatus(turnId);
      const signature = turnSignature(turn);

      if (signature !== previousSignature) {
        previousSignature = signature;
        yield* emitEvent(
          {
            method: "turn/status",
            params: {
              thread_id: this.id,
              turn_id: turnId,
              turn,
            },
            timestamp: new Date().toISOString(),
          },
          options.onEvent,
        );
      }

      if (isTerminalStatus(turn.status)) {
        yield* emitEvent(
          {
            method: "turn/completed",
            params: {
              thread_id: this.id,
              turn_id: turnId,
              status: turn.status,
              token_usage: turn.token_usage ?? null,
            },
            timestamp: new Date().toISOString(),
          },
          options.onEvent,
        );
        return;
      }

      if (Date.now() - startedAt >= timeoutMs) {
        yield* emitEvent(
          {
            method: "turn/timeout",
            params: {
              thread_id: this.id,
              turn_id: turnId,
              timeout_ms: timeoutMs,
            },
            timestamp: new Date().toISOString(),
          },
          options.onEvent,
        );
        return;
      }

      await sleep(pollIntervalMs);
    }
  }
}

function normalizeBaseUrl(value: string | undefined): string {
  return (value ?? "http://127.0.0.1:8080").replace(/\/+$/, "");
}

function resolveFetch(customFetch: FetchLike | undefined): FetchLike {
  if (customFetch) {
    return customFetch;
  }

  const globalFetch = (globalThis as { fetch?: FetchLike }).fetch;
  if (!globalFetch) {
    throw new Error("No fetch implementation available. Pass `fetch` in HarnessOptions.");
  }
  return globalFetch;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function isTerminalStatus(status: string): boolean {
  return status === "completed" || status === "cancelled" || status === "failed";
}

function turnSignature(turn: TurnSnapshot): string {
  const itemCount = Array.isArray(turn.items) ? turn.items.length : 0;
  return `${turn.status}|${itemCount}|${turn.completed_at ?? ""}`;
}

function extractOutput(turn: TurnSnapshot | undefined): string {
  if (!turn || !Array.isArray(turn.items)) {
    return "";
  }

  const messages: string[] = [];
  for (const item of turn.items) {
    if (item.type === "user_message") {
      continue;
    }
    if (typeof item.content === "string" && item.content.trim()) {
      messages.push(item.content.trim());
      continue;
    }
    if (typeof item.stdout === "string" && item.stdout.trim()) {
      messages.push(item.stdout.trim());
      continue;
    }
    if (typeof item.message === "string" && item.message.trim()) {
      messages.push(item.message.trim());
    }
  }

  return messages.join("\n\n");
}

async function withTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number,
  timeoutMessage: string,
): Promise<T> {
  let timeoutHandle: NodeJS.Timeout | undefined;
  const timeoutPromise = new Promise<T>((_, reject) => {
    timeoutHandle = setTimeout(() => reject(new Error(timeoutMessage)), timeoutMs);
  });

  try {
    return await Promise.race([promise, timeoutPromise]);
  } finally {
    if (timeoutHandle) {
      clearTimeout(timeoutHandle);
    }
  }
}

async function* emitEvent(
  event: ThreadEvent,
  onEvent: ((event: ThreadEvent) => void | Promise<void>) | undefined,
): AsyncGenerator<ThreadEvent, void, void> {
  if (onEvent) {
    await onEvent(event);
  }
  yield event;
}
