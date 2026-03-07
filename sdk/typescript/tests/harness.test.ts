import assert from "node:assert/strict";
import test from "node:test";
import {
  Harness,
  HarnessRpcError,
  SDK_TURN_COMPLETED,
  SDK_TURN_TIMEOUT,
  ThreadEvent,
} from "../src/index";

interface RecordedCall {
  url: string;
  method: string;
  params: Record<string, unknown>;
}

function createMockFetch(
  handler: (method: string, params: Record<string, unknown>) => unknown,
): {
  calls: RecordedCall[];
  fetch: (
    url: string,
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
    fetch: async (url, init) => {
      const payload = JSON.parse(init.body) as {
        method: string;
        params: Record<string, unknown>;
        id: number;
      };

      calls.push({ url, method: payload.method, params: payload.params });
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

test("startThread sends configured cwd", async () => {
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

test("startThread omits cwd when not configured and defaults to port 9800", async () => {
  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-2" } };
    }
    return { result: {} };
  });

  const harness = new Harness({ fetch: mock.fetch });
  await harness.startThread();

  assert.equal(mock.calls[0]?.url, "http://127.0.0.1:9800/rpc");
  assert.equal(Object.prototype.hasOwnProperty.call(mock.calls[0]?.params ?? {}, "cwd"), false);
});

test("run returns completed status and extracted output", async () => {
  let statusPollCount = 0;

  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-3" } };
    }
    if (method === "turn/start") {
      return { result: { turn_id: "turn-3" } };
    }
    if (method === "turn/status") {
      statusPollCount += 1;
      if (statusPollCount === 1) {
        return {
          result: {
            id: "turn-3",
            thread_id: "thread-3",
            status: "running",
            items: [{ type: "user_message", content: "hello" }],
          },
        };
      }
      return {
        result: {
          id: "turn-3",
          thread_id: "thread-3",
          status: "completed",
          items: [
            { type: "user_message", content: "hello" },
            { type: "agent_reasoning", content: "done" },
            { type: "shell_command", command: "ls", stdout: "ls output", stderr: "" },
            { type: "error", code: 1, message: "tool failed" },
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

  assert.equal(result.threadId, "thread-3");
  assert.equal(result.turnId, "turn-3");
  assert.equal(result.status, "completed");
  assert.equal(result.output, "done\n\nls output\n\ntool failed");
  assert.equal(result.timedOut, false);
  assert.ok(result.events.some((event) => event.method === SDK_TURN_COMPLETED));
  assert.ok(emitted.length >= 3);
});

test("run handles timeout when turn never reaches terminal status", async () => {
  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-4" } };
    }
    if (method === "turn/start") {
      return { result: { turn_id: "turn-4" } };
    }
    if (method === "turn/status") {
      return {
        result: {
          id: "turn-4",
          thread_id: "thread-4",
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

  assert.equal(result.threadId, "thread-4");
  assert.equal(result.turnId, "turn-4");
  assert.equal(result.status, "running");
  assert.equal(result.timedOut, true);
  assert.ok(result.events.some((event) => event.method === SDK_TURN_TIMEOUT));
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
