import type { Harness } from "./client";
import type { RunOptions, RunResult, ThreadEvent, TurnSnapshot } from "./types";
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

async function* emitEvent(
  event: ThreadEvent,
  onEvent: ((event: ThreadEvent) => void | Promise<void>) | undefined,
): AsyncGenerator<ThreadEvent, void, void> {
  if (onEvent) {
    await onEvent(event);
  }
  yield event;
}
