import assert from "node:assert/strict";
import test from "node:test";
import { Harness, HarnessRpcError, ThreadEvent } from "../src/index";

interface RecordedCall {
  method: string;
  params: Record<string, unknown>;
}

function createMockFetch(
  handler: (method: string, params: Record<string, unknown>) => unknown,
): {
  calls: RecordedCall[];
  fetch: (
    _url: string,
    init: {
      method: string;
      headers: Record<string, string>;
      body: string;
    },
  ) => Promise<{ ok: boolean; status: number; text(): Promise<string> }>;
} {
  const calls: RecordedCall[] = [];

  return {
    calls,
    fetch: async (_url, init) => {
      const payload = JSON.parse(init.body) as {
        method: string;
        params: Record<string, unknown>;
        id: number;
      };

      calls.push({ method: payload.method, params: payload.params });
      const result = handler(payload.method, payload.params);
      const envelope = {
        jsonrpc: "2.0",
        id: payload.id,
        ...result,
      };

      return {
        ok: true,
        status: 200,
        async text() {
          return JSON.stringify(envelope);
        },
      };
    },
  };
}

test("startThread sends thread/start and returns thread handle", async () => {
  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-1" } };
    }
    return { result: {} };
  });

  const harness = new Harness({ fetch: mock.fetch, cwd: "/repo" });
  const thread = await harness.startThread();

  assert.equal(thread.id, "thread-1");
  assert.equal(mock.calls.length, 1);
  assert.equal(mock.calls[0]?.method, "thread/start");
  assert.equal(mock.calls[0]?.params.cwd, "/repo");
});

test("run returns completed status and collected output", async () => {
  let statusPollCount = 0;

  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-2" } };
    }
    if (method === "turn/start") {
      return { result: { turn_id: "turn-2" } };
    }
    if (method === "turn/status") {
      statusPollCount += 1;
      if (statusPollCount === 1) {
        return {
          result: {
            id: "turn-2",
            thread_id: "thread-2",
            status: "running",
            items: [{ type: "user_message", content: "hello" }],
          },
        };
      }
      return {
        result: {
          id: "turn-2",
          thread_id: "thread-2",
          status: "completed",
          items: [
            { type: "user_message", content: "hello" },
            { type: "agent_reasoning", content: "done" },
          ],
          token_usage: {
            input_tokens: 1,
            output_tokens: 1,
            total_tokens: 2,
            cost_usd: 0,
          },
        },
      };
    }
    return { result: {} };
  });

  const harness = new Harness({
    fetch: mock.fetch,
    defaultPollIntervalMs: 1,
    defaultRunTimeoutMs: 500,
  });
  const thread = await harness.startThread({ cwd: "/repo" });
  const emitted: ThreadEvent[] = [];
  const result = await thread.run("Summarize", {
    onEvent: async (event) => {
      emitted.push(event);
    },
  });

  assert.equal(result.threadId, "thread-2");
  assert.equal(result.turnId, "turn-2");
  assert.equal(result.status, "completed");
  assert.equal(result.output, "done");
  assert.equal(result.timedOut, false);
  assert.ok(result.events.some((event) => event.method === "turn/completed"));
  assert.ok(emitted.length >= 3);
});

test("run handles timeout when turn never reaches terminal status", async () => {
  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-3" } };
    }
    if (method === "turn/start") {
      return { result: { turn_id: "turn-3" } };
    }
    if (method === "turn/status") {
      return {
        result: {
          id: "turn-3",
          thread_id: "thread-3",
          status: "running",
          items: [{ type: "user_message", content: "hello" }],
        },
      };
    }
    return { result: {} };
  });

  const harness = new Harness({
    fetch: mock.fetch,
    defaultPollIntervalMs: 1,
    defaultRunTimeoutMs: 5,
  });

  const thread = await harness.startThread({ cwd: "/repo" });
  const result = await thread.run("Keep going");

  assert.equal(result.threadId, "thread-3");
  assert.equal(result.turnId, "turn-3");
  assert.equal(result.status, "running");
  assert.equal(result.timedOut, true);
  assert.ok(result.events.some((event) => event.method === "turn/timeout"));
});

test("raises HarnessRpcError when server returns JSON-RPC error", async () => {
  const mock = createMockFetch(() => ({
    error: { code: -32001, message: "thread not found" },
  }));

  const harness = new Harness({ fetch: mock.fetch });
  await assert.rejects(() => harness.resumeThread("missing-thread"), (error) => {
    assert.ok(error instanceof HarnessRpcError);
    assert.equal(error.code, -32001);
    return true;
  });
});
