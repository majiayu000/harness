import type { Harness } from "./client";
import type {
  RunOptions,
  RunResult,
  ThreadEvent,
  TokenUsage,
  TurnItem,
  TurnSnapshot,
  TurnStatus,
} from "./types";
import { extractOutput, isTerminalStatus, sleep, turnSignature } from "./utils";

export const SDK_TURN_STARTED = "sdk:turn/started";
export const SDK_TURN_STATUS = "sdk:turn/status";
export const SDK_TURN_COMPLETED = "sdk:turn/completed";
export const SDK_TURN_TIMEOUT = "sdk:turn/timeout";

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

      if (event.method === SDK_TURN_STARTED) {
        turnId = String(event.params.turn_id ?? "");
      }

      if (event.method === SDK_TURN_STATUS) {
        const turn = parseTurnSnapshot(event.params.turn);
        if (turn) {
          latestTurn = turn;
        }
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
      timedOut: events.some((event) => event.method === SDK_TURN_TIMEOUT),
    };
  }

  async *runStream(
    prompt: string,
    options: RunOptions = {},
  ): AsyncGenerator<ThreadEvent, void, void> {
    const pollIntervalMs = this.client.resolvePollIntervalMs(options.pollIntervalMs);
    const timeoutMs = this.client.resolveRunTimeoutMs(options.timeoutMs);
    const startedAt = Date.now();
    let previousSignature = "";

    const started = await this.client.startTurn(this.id, prompt);
    const turnId = started.turn_id;

    yield* emitEvent(
      {
        method: SDK_TURN_STARTED,
        params: {
          thread_id: this.id,
          turn_id: turnId,
          source: "sdk-poll",
          server_method: "turn/start",
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
            method: SDK_TURN_STATUS,
            params: {
              thread_id: this.id,
              turn_id: turnId,
              turn,
              source: "sdk-poll",
              server_method: "turn/status",
            },
            timestamp: new Date().toISOString(),
          },
          options.onEvent,
        );
      }

      if (isTerminalStatus(turn.status)) {
        yield* emitEvent(
          {
            method: SDK_TURN_COMPLETED,
            params: {
              thread_id: this.id,
              turn_id: turnId,
              status: turn.status,
              token_usage: turn.token_usage ?? null,
              source: "sdk-poll",
              server_method: "turn/status",
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
            method: SDK_TURN_TIMEOUT,
            params: {
              thread_id: this.id,
              turn_id: turnId,
              timeout_ms: timeoutMs,
              source: "sdk-poll",
              server_method: "turn/status",
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

async function* emitEvent<TEvent extends ThreadEvent>(
  event: TEvent,
  onEvent: ((event: ThreadEvent) => void | Promise<void>) | undefined,
): AsyncGenerator<TEvent, void, void> {
  if (onEvent) {
    await onEvent(event);
  }
  yield event;
}

function parseTurnSnapshot(value: unknown): TurnSnapshot | undefined {
  if (!isRecord(value)) {
    return undefined;
  }

  if (
    typeof value.id !== "string" ||
    typeof value.thread_id !== "string" ||
    !isTurnStatus(value.status) ||
    !Array.isArray(value.items)
  ) {
    return undefined;
  }

  const items = parseTurnItems(value.items);
  if (!items) {
    return undefined;
  }

  const snapshot: TurnSnapshot = {
    id: value.id,
    thread_id: value.thread_id,
    status: value.status,
    items,
  };

  if (isNullableString(value.agent_id)) {
    snapshot.agent_id = value.agent_id;
  }
  if (isNullableString(value.started_at)) {
    snapshot.started_at = value.started_at;
  }
  if (isNullableString(value.completed_at)) {
    snapshot.completed_at = value.completed_at;
  }

  const tokenUsage = parseTokenUsage(value.token_usage);
  if (tokenUsage) {
    snapshot.token_usage = tokenUsage;
  }

  return snapshot;
}

function parseTurnItems(items: unknown[]): TurnItem[] | undefined {
  const parsedItems: TurnItem[] = [];
  for (const item of items) {
    const parsedItem = parseTurnItem(item);
    if (!parsedItem) {
      return undefined;
    }
    parsedItems.push(parsedItem);
  }
  return parsedItems;
}

function parseTurnItem(value: unknown): TurnItem | undefined {
  if (!isRecord(value) || typeof value.type !== "string") {
    return undefined;
  }

  switch (value.type) {
    case "user_message":
    case "agent_reasoning":
      if (typeof value.content !== "string") {
        return undefined;
      }
      return { type: value.type, content: value.content };
    case "shell_command": {
      if (
        typeof value.command !== "string" ||
        typeof value.stdout !== "string" ||
        typeof value.stderr !== "string"
      ) {
        return undefined;
      }
      return {
        type: "shell_command",
        command: value.command,
        stdout: value.stdout,
        stderr: value.stderr,
        ...(typeof value.exit_code === "number" || value.exit_code === null
          ? { exit_code: value.exit_code }
          : {}),
      };
    }
    case "file_edit":
      if (
        typeof value.path !== "string" ||
        typeof value.before !== "string" ||
        typeof value.after !== "string"
      ) {
        return undefined;
      }
      return { type: "file_edit", path: value.path, before: value.before, after: value.after };
    case "file_read":
      if (typeof value.path !== "string" || typeof value.content !== "string") {
        return undefined;
      }
      return { type: "file_read", path: value.path, content: value.content };
    case "tool_call":
      if (typeof value.name !== "string") {
        return undefined;
      }
      return {
        type: "tool_call",
        name: value.name,
        input: value.input,
        output: value.output,
      };
    case "approval_request": {
      if (typeof value.action !== "string") {
        return undefined;
      }
      return {
        type: "approval_request",
        action: value.action,
        ...(typeof value.approved === "boolean" || value.approved === null
          ? { approved: value.approved }
          : {}),
      };
    }
    case "error":
      if (typeof value.code !== "number" || typeof value.message !== "string") {
        return undefined;
      }
      return { type: "error", code: value.code, message: value.message };
    default:
      return { ...value, type: value.type };
  }
}

function parseTokenUsage(value: unknown): TokenUsage | undefined {
  if (!isRecord(value)) {
    return undefined;
  }

  if (
    typeof value.input_tokens !== "number" ||
    typeof value.output_tokens !== "number" ||
    typeof value.total_tokens !== "number" ||
    typeof value.cost_usd !== "number"
  ) {
    return undefined;
  }

  return {
    input_tokens: value.input_tokens,
    output_tokens: value.output_tokens,
    total_tokens: value.total_tokens,
    cost_usd: value.cost_usd,
  };
}

function isTurnStatus(value: unknown): value is TurnStatus {
  return value === "running" || value === "completed" || value === "cancelled" || value === "failed";
}

function isNullableString(value: unknown): value is string | null {
  return typeof value === "string" || value === null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
