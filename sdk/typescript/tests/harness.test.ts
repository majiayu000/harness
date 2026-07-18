import assert from "node:assert/strict";
import test from "node:test";
import {
  Harness,
  HarnessHttpError,
  SDK_TURN_COMPLETED,
  SDK_TURN_TIMEOUT,
  type ThreadEvent,
  type TurnSnapshot,
} from "../src/index";
import { turnSignature } from "../src/utils";

interface RecordedCall {
  url: string;
  method: string;
  body: Record<string, unknown> | undefined;
  headers: Record<string, string>;
}

interface MockResponse {
  status?: number;
  body: unknown;
}

function createMockFetch(handler: (call: RecordedCall) => MockResponse) {
  const calls: RecordedCall[] = [];
  return {
    calls,
    fetch: async (
      url: string,
      init: {
        method: string;
        headers: Record<string, string>;
        body?: string;
      },
    ) => {
      const call: RecordedCall = {
        url,
        method: init.method,
        body: init.body ? (JSON.parse(init.body) as Record<string, unknown>) : undefined,
        headers: init.headers,
      };
      calls.push(call);
      const response = handler(call);
      const status = response.status ?? 200;
      return {
        ok: status >= 200 && status < 300,
        status,
        async text() {
          return JSON.stringify(response.body);
        },
      };
    },
  };
}

test("startThread creates a local project handle without a server request", async () => {
  const mock = createMockFetch(() => ({ body: {} }));
  const harness = new Harness({ fetch: mock.fetch, cwd: "/repo" });

  const thread = await harness.startThread();

  assert.equal(thread.id, "/repo");
  assert.equal(mock.calls.length, 0);
});

test("turn signatures change when a pending approval request changes", () => {
  const turn = (id: string): TurnSnapshot => ({
    id: "submission-1",
    thread_id: "/repo",
    status: "running",
    items: [
      {
        type: "approval_request",
        id,
        action: "run tests",
        approved: null,
      },
    ],
  });

  assert.notEqual(turnSignature(turn("request-1")), turnSignature(turn("request-2")));
});

test("startThread requires a project root", async () => {
  const mock = createMockFetch(() => ({ body: {} }));
  const harness = new Harness({ fetch: mock.fetch });

  await assert.rejects(
    () => harness.startThread(),
    /`cwd` is required; pass Harness\(\{ cwd \}\) or startThread\(\{ cwd \}\)\./,
  );
});

test("run submits and polls a workflow-runtime prompt", async () => {
  let detailPolls = 0;
  const mock = createMockFetch((call) => {
    if (call.method === "POST" && call.url.endsWith("/api/workflows/runtime/submissions")) {
      return {
        status: 202,
        body: {
          task_id: "submission-1",
          submission_id: "submission-1",
          execution_path: "workflow_runtime",
        },
      };
    }
    if (call.url.endsWith("/api/workflows/runtime/submissions/submission-1")) {
      detailPolls += 1;
      return {
        body: {
          submission_id: "submission-1",
          status: detailPolls === 1 ? "implementing" : "done",
          project: "/repo",
          created_at: "2026-07-18T00:00:00Z",
          updated_at: "2026-07-18T00:00:01Z",
          token_usage: {
            input_tokens: 120,
            output_tokens: 30,
            total_tokens: 150,
            cost_usd: 0.02,
          },
        },
      };
    }
    if (call.url.endsWith("/api/workflows/runtime/submissions/submission-1/artifacts")) {
      return {
        body: [
          {
            artifact_type: "activity_result_envelope",
            content: JSON.stringify({
              final_result: { summary: "Repository analyzed.", error: null },
            }),
          },
        ],
      };
    }
    throw new Error(`unexpected request: ${call.method} ${call.url}`);
  });
  const harness = new Harness({
    fetch: mock.fetch,
    cwd: "/repo",
    apiToken: "token-123",
    defaultPollIntervalMs: 1,
    defaultRunTimeoutMs: 500,
  });
  const thread = await harness.startThread();
  const emitted: ThreadEvent[] = [];

  const result = await thread.run("Analyze the repository", {
    onEvent: (event) => emitted.push(event),
  });

  assert.equal(result.turnId, "submission-1");
  assert.equal(result.status, "completed");
  assert.equal(result.output, "Repository analyzed.");
  assert.equal(result.timedOut, false);
  assert.ok(result.events.some((event) => event.method === SDK_TURN_COMPLETED));
  const completed = result.events.find((event) => event.method === SDK_TURN_COMPLETED);
  assert.deepEqual(completed?.params.token_usage, {
    input_tokens: 120,
    output_tokens: 30,
    total_tokens: 150,
    cost_usd: 0.02,
  });
  assert.equal(emitted.length, result.events.length);
  assert.deepEqual(mock.calls[0]?.body, {
    project: "/repo",
    prompt: "Analyze the repository",
  });
  assert.equal(mock.calls[0]?.headers.authorization, "Bearer token-123");
});

test("run exposes a terminal runtime error", async () => {
  const mock = createMockFetch((call) => {
    if (call.method === "POST") {
      return {
        status: 202,
        body: {
          task_id: "submission-failed",
          execution_path: "workflow_runtime",
        },
      };
    }
    if (call.url.endsWith("/artifacts")) {
      return {
        body: [
          {
            artifact_type: "activity_result_envelope",
            content: JSON.stringify({
              final_result: { summary: "Agent failed.", error: "spawn failed" },
            }),
          },
        ],
      };
    }
    return {
      body: {
        submission_id: "submission-failed",
        status: "failed",
        project: "/repo",
      },
    };
  });
  const harness = new Harness({ fetch: mock.fetch, cwd: "/repo" });

  const result = await (await harness.startThread()).run("Fail");

  assert.equal(result.status, "failed");
  assert.equal(result.output, "Agent failed.\n\nspawn failed");
});

test("run falls back to the task error when artifacts are not found", async () => {
  const mock = createMockFetch((call) => {
    if (call.method === "POST") {
      return {
        status: 202,
        body: {
          task_id: "submission-failed-early",
          execution_path: "workflow_runtime",
        },
      };
    }
    if (call.url.endsWith("/artifacts")) {
      return { status: 404, body: { error: "runtime submission not found" } };
    }
    return {
      body: {
        submission_id: "submission-failed-early",
        status: "failed",
        project: "/repo",
        error: "startup failed",
      },
    };
  });
  const harness = new Harness({ fetch: mock.fetch, cwd: "/repo" });

  const result = await (await harness.startThread()).run("Fail");

  assert.equal(result.status, "failed");
  assert.equal(result.output, "startup failed");
});

test("run rejects a non-object runtime submission detail", async () => {
  const mock = createMockFetch((call) => {
    if (call.method === "POST") {
      return {
        status: 202,
        body: { task_id: "submission-invalid-detail", execution_path: "workflow_runtime" },
      };
    }
    return { body: null };
  });
  const harness = new Harness({ fetch: mock.fetch, cwd: "/repo" });

  await assert.rejects(
    () => harness.startThread().then((thread) => thread.run("Run")),
    /runtime submission detail must be an object/,
  );
});

test("run rejects a non-array runtime artifacts response", async () => {
  const mock = createMockFetch((call) => {
    if (call.method === "POST") {
      return {
        status: 202,
        body: {
          task_id: "submission-invalid-artifacts",
          execution_path: "workflow_runtime",
        },
      };
    }
    if (call.url.endsWith("/artifacts")) {
      return { body: { artifact_type: "activity_result_envelope" } };
    }
    return {
      body: {
        submission_id: "submission-invalid-artifacts",
        status: "done",
        project: "/repo",
      },
    };
  });
  const harness = new Harness({ fetch: mock.fetch, cwd: "/repo" });

  await assert.rejects(
    () => harness.startThread().then((thread) => thread.run("Run")),
    /runtime artifacts response must be an array/,
  );
});

test("run emits timeout while a runtime submission remains active", async () => {
  const mock = createMockFetch((call) => {
    if (call.method === "POST") {
      return {
        status: 202,
        body: { task_id: "submission-running", execution_path: "workflow_runtime" },
      };
    }
    return {
      body: {
        submission_id: "submission-running",
        status: "implementing",
        project: "/repo",
        pending_approvals: [
          {
            type: "approval_request",
            id: "request-1",
            action: "run cargo test",
            approved: null,
          },
        ],
      },
    };
  });
  const harness = new Harness({
    fetch: mock.fetch,
    cwd: "/repo",
    defaultPollIntervalMs: 1,
    defaultRunTimeoutMs: 5,
  });

  const result = await (await harness.startThread()).run("Keep going");

  assert.equal(result.status, "running");
  assert.equal(result.timedOut, true);
  assert.deepEqual(result.turn?.items, [
    {
      type: "approval_request",
      id: "request-1",
      action: "run cargo test",
      approved: null,
    },
  ]);
  assert.ok(result.events.some((event) => event.method === SDK_TURN_TIMEOUT));
  assert.ok(mock.calls.every((call) => !call.url.endsWith("/artifacts")));
});

test("run rejects malformed pending approvals", async () => {
  const mock = createMockFetch((call) => {
    if (call.method === "POST") {
      return {
        status: 202,
        body: { task_id: "submission-running", execution_path: "workflow_runtime" },
      };
    }
    return {
      body: {
        submission_id: "submission-running",
        status: "implementing",
        project: "/repo",
        pending_approvals: { id: "request-1" },
      },
    };
  });
  const harness = new Harness({ fetch: mock.fetch, cwd: "/repo" });

  await assert.rejects(
    () => harness.startThread().then((thread) => thread.run("Keep going")),
    /runtime pending_approvals must be an array/,
  );
});

test("resumeThread reuses a project handle locally", async () => {
  const mock = createMockFetch(() => ({ body: {} }));
  const harness = new Harness({ fetch: mock.fetch });

  const thread = await harness.resumeThread("/repo");

  assert.equal(thread.id, "/repo");
  assert.equal(mock.calls.length, 0);
});

test("HTTP failures expose status and response data", async () => {
  const mock = createMockFetch(() => ({
    status: 503,
    body: { error: "workflow runtime store unavailable" },
  }));
  const harness = new Harness({ fetch: mock.fetch, cwd: "/repo" });

  await assert.rejects(
    () => harness.startTurn("/repo", "Run"),
    (error) => {
      assert.ok(error instanceof HarnessHttpError);
      assert.equal(error.status, 503);
      assert.deepEqual(error.data, { error: "workflow runtime store unavailable" });
      return true;
    },
  );
});

test("malformed runtime result artifacts fail loudly", async () => {
  const mock = createMockFetch((call) => {
    if (call.method === "POST") {
      return {
        status: 202,
        body: { task_id: "submission-bad", execution_path: "workflow_runtime" },
      };
    }
    if (call.url.endsWith("/artifacts")) {
      return {
        body: [{ artifact_type: "activity_result_envelope", content: "not-json" }],
      };
    }
    return {
      body: { submission_id: "submission-bad", status: "done", project: "/repo" },
    };
  });
  const harness = new Harness({ fetch: mock.fetch, cwd: "/repo" });

  await assert.rejects(
    () => harness.startThread().then((thread) => thread.run("Run")),
    /Invalid activity_result_envelope artifact/,
  );
});

test("non-object runtime result artifacts fail loudly", async () => {
  const mock = createMockFetch((call) => {
    if (call.method === "POST") {
      return {
        status: 202,
        body: { task_id: "submission-null", execution_path: "workflow_runtime" },
      };
    }
    if (call.url.endsWith("/artifacts")) {
      return {
        body: [{ artifact_type: "activity_result_envelope", content: "null" }],
      };
    }
    return {
      body: { submission_id: "submission-null", status: "done", project: "/repo" },
    };
  });
  const harness = new Harness({ fetch: mock.fetch, cwd: "/repo" });

  await assert.rejects(
    () => harness.startThread().then((thread) => thread.run("Run")),
    /Invalid activity_result_envelope artifact: expected an object/,
  );
});
