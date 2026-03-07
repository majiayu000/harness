import { HarnessThread } from "./thread";
import { RpcTransport } from "./transport";
import type { HarnessOptions, StartThreadOptions, TurnSnapshot } from "./types";

export class Harness {
  private readonly transport: RpcTransport;
  private readonly defaultCwd: string | undefined;
  private readonly defaultPollIntervalMs: number;
  private readonly defaultRunTimeoutMs: number;

  constructor(options: HarnessOptions = {}) {
    this.transport = new RpcTransport(options);
    this.defaultCwd = options.cwd;
    this.defaultPollIntervalMs = options.defaultPollIntervalMs ?? 500;
    this.defaultRunTimeoutMs = options.defaultRunTimeoutMs ?? 30_000;
  }

  async startThread(options: StartThreadOptions = {}): Promise<HarnessThread> {
    const cwd = options.cwd ?? this.defaultCwd;
    if (typeof cwd !== "string" || cwd.length === 0) {
      throw new Error("`cwd` is required for thread/start; pass Harness({ cwd }) or startThread({ cwd }).");
    }

    const result = await this.transport.request<{ thread_id: string }>("thread/start", {
      cwd,
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

  resolvePollIntervalMs(value: number | undefined): number {
    return Math.max(10, value ?? this.defaultPollIntervalMs);
  }

  resolveRunTimeoutMs(value: number | undefined): number {
    return Math.max(1, value ?? this.defaultRunTimeoutMs);
  }
}
