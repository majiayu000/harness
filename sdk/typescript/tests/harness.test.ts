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
  headers: Record<string, string>;
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

      calls.push({
        url,
        method: payload.method,
        params: payload.params,
        headers: init.headers,
      });
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

test("startThread requires cwd when not configured", async () => {
  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-2" } };
    }
    return { result: {} };
  });

  const harness = new Harness({ fetch: mock.fetch });
  await assert.rejects(
    () => harness.startThread(),
    /`cwd` is required for thread\/start; pass Harness\(\{ cwd \}\) or startThread\(\{ cwd \}\)\./,
  );
  assert.equal(mock.calls.length, 0);
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

test("run validates status-event turn payload before storing snapshot", async () => {
  let statusPollCount = 0;

  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-5" } };
    }
    if (method === "turn/start") {
      return { result: { turn_id: "turn-5" } };
    }
    if (method === "turn/status") {
      statusPollCount += 1;
      if (statusPollCount === 1) {
        return {
          result: {
            id: "turn-5",
            thread_id: "thread-5",
            status: "completed",
            items: "invalid-items",
          },
        };
      }
      return {
        result: {
          id: "turn-5",
          thread_id: "thread-5",
          status: "completed",
          items: [{ type: "agent_reasoning", content: "final answer" }],
        },
      };
    }
    return { result: {} };
  });

  const harness = new Harness({
    fetch: mock.fetch,
    cwd: "/repo",
    defaultPollIntervalMs: 1,
    defaultRunTimeoutMs: 500,
  });

  const thread = await harness.startThread();
  const result = await thread.run("Summarize");

  assert.equal(result.turnId, "turn-5");
  assert.equal(result.status, "completed");
  assert.equal(result.output, "final answer");
  assert.equal(statusPollCount, 2);
});

test("run preserves approval_request id in final snapshot", async () => {
  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-6" } };
    }
    if (method === "turn/start") {
      return { result: { turn_id: "turn-6" } };
    }
    if (method === "turn/status") {
      return {
        result: {
          id: "turn-6",
          thread_id: "thread-6",
          status: "completed",
          items: [
            { type: "user_message", content: "hello" },
            {
              type: "approval_request",
              id: "approval-1",
              action: "execute_command",
              approved: null,
            },
          ],
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
  const result = await thread.run("Summarize");

  assert.equal(result.status, "completed");
  assert.equal(result.turn?.items.length, 2);
  assert.deepEqual(result.turn?.items[1], {
    type: "approval_request",
    id: "approval-1",
    action: "execute_command",
    approved: null,
  });
});

test("run emits timeout event with timeout_ms", async () => {
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
  const timeoutEvent = result.events.find((event) => event.method === SDK_TURN_TIMEOUT);

  assert.equal(result.threadId, "thread-4");
  assert.equal(result.turnId, "turn-4");
  assert.equal(result.status, "running");
  assert.equal(result.timedOut, true);
  assert.ok(timeoutEvent);
  assert.equal(timeoutEvent?.params.timeout_ms, 5);
  assert.equal("timeout_seconds" in timeoutEvent!.params, false);
});

test("run preserves unknown turn item types in final snapshot", async () => {
  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-7" } };
    }
    if (method === "turn/start") {
      return { result: { turn_id: "turn-7" } };
    }
    if (method === "turn/status") {
      return {
        result: {
          id: "turn-7",
          thread_id: "thread-7",
          status: "completed",
          items: [
            { type: "user_message", content: "hello" },
            { type: "future_item", payload: { a: 1 } },
          ],
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
  const result = await thread.run("Summarize");

  assert.equal(result.status, "completed");
  assert.equal(result.turn?.items.length, 2);
  assert.equal(result.turn?.items[1]?.type, "future_item");
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

test("adds bearer token header when apiToken is configured", async () => {
  const mock = createMockFetch((method) => {
    if (method === "thread/start") {
      return { result: { thread_id: "thread-auth" } };
    }
    return { result: {} };
  });

  const harness = new Harness({
    fetch: mock.fetch,
    cwd: "/repo",
    apiToken: "token-123",
  });
  await harness.startThread();

  assert.equal(mock.calls.length, 1);
  assert.equal(mock.calls[0]?.headers.authorization, "Bearer token-123");
});
