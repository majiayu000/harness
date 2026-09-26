import { readFileSync } from "node:fs";
import { join } from "node:path";
import { runInNewContext } from "node:vm";
import { expect, it } from "vitest";

const asset = (name) => readFileSync(join(process.cwd(), "public/console-v2", name), "utf8");

async function setup(beforeFetch = async () => {}) {
  const flags = { readerCancelled: false, approvalsFail: false, streamFail: false, streamEOF: false, monitorFail: false };
  const calls = [];
  const rpcMethods = [];
  const rpcCalls = [];
  const task = {
    id: "sub-1", workflow: { id: "wf-1", state: "implementing" }, repo: "owner/repo",
    external_id: "42", description: "Fix issue 42", status: "implementing", turn: 2, project: "/tmp/repo",
  };
  const responses = {
    "/api/workflows/runtime/submissions?limit=200&active=true": { data: [task], page: { has_more: false } },
    "/api/workflows/runtime/submissions?limit=200&status=done%2Cfailed%2Ccancelled": { data: [{ id: "old-1", workflow: { id: "old-wf-1", state: "done" }, status: "done", created_at: "2020-01-01T00:00:00Z" }], page: { has_more: true, next_cursor: "next" } },
    "/api/workflows/runtime/submissions?limit=200&status=done%2Cfailed%2Ccancelled&cursor=next": { data: [{ id: "old-2", workflow: { id: "old-wf-2", state: "done" }, status: "done", created_at: "2020-01-02T00:00:00Z" }], page: { has_more: false } },
    "/api/workflows/runtime/approvals": { data: [{ submission_id: "sub-1", pending_approvals: [{ type: "approval_request", id: "request-1", action: "run tests", approved: null }] }] },
    "/api/operator-monitor": { health: { status: "ok", degraded_subsystems: [], uptime_secs: 30 }, failures: [], operator_actions: [], activity: {} },
    "/api/overview": { projects: [{ id: "/tmp/repo", root: "/tmp/repo", merged_24h: 0 }], runtimes: [] },
    "/api/usage-monitor": {
      summary: { total_tokens: 100, cache_read_input_tokens: 0, request_count: 1 },
      agent_invocations: [
        { workflow_id: "wf-1", agent_runtime: "current-agent", lease_state: "active_leased", activity: "implement_issue" },
        { workflow_id: "wf-1", agent_runtime: "old-agent", lease_state: "released", activity: "plan_issue" },
      ],
    },
    "/api/workflows/runtime/tree?summary_only=true": { summary: { circuit_breakers: [] } },
    "/api/dashboard": { global: { max_concurrent: 2 }, runtime_hosts: [] },
    "/api/operator-snapshot": {},
    "/api/worktrees": [],
    "/api/intake": { channels: [] },
    "/projects": [],
    "/api/projects/owner%2Frepo/memory": { records: [] },
    "/api/token-usage": { by_hour: { "2026-09-25T12": { input_tokens: 100 } } },
  };
  const context = {
    URLSearchParams, TextDecoder, AbortController, setTimeout, clearTimeout, console,
    sessionStorage: { getItem: () => null },
    location: { origin: "http://localhost" },
    addEventListener: () => {},
    setInterval: () => 1,
    fetch: async (path, init) => {
      calls.push(path);
      const override = await beforeFetch(path, init);
      if (override) return override;
      if (path.endsWith("/stream")) {
        if (flags.streamFail) return { ok: false, status: 503 };
        const data = new TextEncoder().encode('data: {"type":"message_delta","text":"hello"}\n\n' + (flags.streamEOF ? '' : 'data: {"type":"done"}\n\n'));
        let sent = false;
        return { ok: true, body: { getReader: () => ({
          read: async () => sent ? { done: true } : ((sent = true), { done: false, value: data }),
          cancel: async () => { flags.readerCancelled = true; },
        }) } };
      }
      if (path === "/rpc") {
        rpcMethods.push(JSON.parse(init.body).method);
        rpcCalls.push(JSON.parse(init.body));
        return { ok: true, status: 200, json: async () => ({ result: [] }) };
      }
      if (flags.approvalsFail && path === "/api/workflows/runtime/approvals") return { ok: false, status: 503, json: async () => ({ error: "approvals unavailable" }) };
      if (flags.monitorFail && path === "/api/operator-monitor") return { ok: false, status: 503, json: async () => ({ error: "monitor unavailable" }) };
      const payload = responses[path];
      return payload === undefined
        ? { ok: false, status: 404, json: async () => ({ error: "missing fixture" }) }
        : { ok: true, status: 200, json: async () => payload };
    },
  };
  context.window = context;
  context.parent = context;
  for (const name of ["harness-console-data.js", "harness-console-data-ext.js", "live-data.js"]) {
    runInNewContext(asset(name), context);
  }
  for (let attempts = 0; context.HC.loading && attempts < 20; attempts++) {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }

  return { context, task, responses, flags, calls, rpcMethods, rpcCalls };
}

it("keeps the current invocation, hourly series, and snake-case stream events", async () => {
  const { context, task, flags, rpcMethods } = await setup();
  await expect.poll(() => context.HC.historyLoading).toBe(false);
  expect(context.HC.workflows[0].agent).toBe("current-agent");
  expect(context.HC.workflows[0].projectId).toBe("/tmp/repo");
  expect(context.HC.workflows[0].lease).toBe("active");
  expect(context.HC.workflows[0].inbox.kind).toBe("approval");
  expect(context.HC.history.map(row => row.id)).toEqual(["old-wf-1", "old-wf-2"]);
  expect(context.HC.X.usage.hourly).toEqual([0.0001]);
  expect(rpcMethods.slice(0, 2)).toEqual(["initialize", "initialized"]);

  await context.HC.refresh();
  expect(context.HC.X.usage.hourly).toEqual([0.0001]);

  flags.approvalsFail = true;
  await context.HC.refresh();
  expect(context.HC.loadError).toContain("approvals unavailable");
  expect(context.HC.workflows[0].inbox.kind).toBe("approval");

  flags.approvalsFail = false;
  task.workflow.state = "awaiting_dependencies";
  await context.HC.refresh();
  expect(context.HC.workflows[0].state).toBe("awaiting_dependencies");

  await context.HC.loadTranscript(context.HC.workflows[0]);
  expect(context.HC.transcripts.get("wf-1")).toEqual([{ t: "hello", c: "oklch(0.86 0.005 275)" }]);
  expect(flags.readerCancelled).toBe(true);

  const retryWorkflow = { id: "retry-wf", submissionId: "sub-1" };
  flags.streamFail = true;
  await context.HC.loadTranscript(retryWorkflow);
  expect(context.HC.transcriptFailed.has("retry-wf")).toBe(true);
  flags.streamFail = false;
  await context.HC.loadTranscript(retryWorkflow);
  expect(context.HC.transcriptFailed.has("retry-wf")).toBe(false);
  expect(context.HC.transcripts.get("retry-wf")).toEqual([{ t: "hello", c: "oklch(0.86 0.005 275)" }]);
});

it("keeps live polling independent of slow history and surfaces history failures", async () => {
  let releaseHistory;
  const pending = new Promise(resolve => { releaseHistory = resolve; });
  const { context, task, responses, flags, calls } = await setup(async path => {
    if (path.includes("status=done")) await pending;
  });
  const H = context.HC;
  expect(H.loading).toBe(false);
  expect(H.historyLoading).toBe(true);
  task.workflow.state = "quality_gate_pending";
  await H.refresh();
  expect(H.workflows[0].state).toBe("quality_gate_pending");
  expect(calls.filter(path => path.includes("active=true"))).toHaveLength(2);
  expect(calls.filter(path => path.includes("status=done"))).toHaveLength(1);

  flags.monitorFail = true;
  task.workflow.state = "implementing";
  await H.refresh();
  expect(H.loadError).toContain("monitor unavailable");
  expect(H.workflows[0].state).toBe("quality_gate_pending");
  expect(H.workflows[0].inbox.kind).toBe("approval");

  releaseHistory();
  await expect.poll(() => H.historyLoading).toBe(false);
  expect(H.workflows[0].state).toBe("quality_gate_pending");
  flags.monitorFail = false;
  await H.refresh();
  expect(H.history).toHaveLength(2);

  delete responses["/api/workflows/runtime/submissions?limit=200&status=done%2Cfailed%2Ccancelled&cursor=next"];
  await H.refresh(true);
  await expect.poll(() => H.historyLoading).toBe(false);
  expect(H.historyError).toContain("missing fixture");
  expect(H.history).toHaveLength(2);
  await H.refresh();
  expect(H.historyError).toContain("missing fixture");
});

it("refreshes history when an active task completes between scheduled history polls", async () => {
  const { context, task, responses } = await setup();
  const H = context.HC;
  await expect.poll(() => H.historyLoading).toBe(false);
  responses["/api/workflows/runtime/submissions?limit=200&active=true"].data = [];
  responses["/api/workflows/runtime/submissions?limit=200&status=done%2Cfailed%2Ccancelled"].data.push({ ...task, workflow: { id: "wf-1", state: "done" } });
  await H.refresh();
  await expect.poll(() => H.historyLoading).toBe(false);
  expect(H.workflows).toHaveLength(0);
  expect(H.history.map(row => row.id)).toContain("wf-1");
});

it("loads details beyond the first 200 project workflows and throttles failed retries", async () => {
  const { context, responses, calls } = await setup();
  const H = context.HC;
  const workflow = H.workflows[0];
  const path = "/api/workflows/runtime/submissions/sub-1";
  responses[path] = { id: "sub-1" };
  responses[path + "/artifacts"] = [];
  responses[path + "/prompts"] = [];
  const tree = offset => "/api/workflows/runtime/tree?detail=full&limit=100&offset=" + offset + "&project_id=%2Ftmp%2Frepo";
  responses[tree(0)] = { workflows: [{ workflow: { id: "another" } }], pagination: { has_more: true } };
  responses[tree(100)] = responses[tree(0)];
  responses[tree(200)] = { workflows: [{ workflow: { id: "wf-1" }, events: [{ event_type: "completed" }] }], pagination: { has_more: false } };
  await H.loadDetails(workflow);
  expect(H.details.get("wf-1").node.events[0].event_type).toBe("completed");
  expect(H.details.get("wf-1").error).toBe("");
  expect(calls).toContain(tree(200));

  H.details.clear();
  delete responses[path];
  await H.loadDetails(workflow);
  expect(H.details.get("wf-1").error).toContain("missing fixture");
  const requests = calls.length;
  await H.loadDetails(workflow);
  expect(calls).toHaveLength(requests);
  responses[path] = { id: "sub-1" };
  H.details.get("wf-1").loadedAt -= 30_001;
  await H.loadDetails(workflow);
  expect(H.details.get("wf-1").error).toBe("");
});


it("queues another history pass when a workflow completes during pagination", async () => {
  let releasePage;
  const page = new Promise(resolve => { releasePage = resolve; });
  const { context, task, responses, calls } = await setup(async path => {
    if (path.includes("status=done") && path.includes("cursor=next")) await page;
  });
  const H = context.HC;
  const firstPage = "/api/workflows/runtime/submissions?limit=200&status=done%2Cfailed%2Ccancelled";
  expect(calls.filter(path => path === firstPage)).toHaveLength(1);
  responses["/api/workflows/runtime/submissions?limit=200&active=true"].data = [];
  responses[firstPage].data.unshift({ ...task, workflow: { id: "wf-1", state: "done" } });
  await H.refresh();
  await H.refresh(true);
  expect(H.historyLoading).toBe(true);
  expect(calls.filter(path => path === firstPage)).toHaveLength(1);
  releasePage();
  await expect.poll(() => H.historyLoading).toBe(false);
  expect(calls.filter(path => path === firstPage)).toHaveLength(2);
  expect(H.history.map(row => row.id)).toContain("wf-1");
});

it("reconnects after transcript EOF without a terminal event", async () => {
  const { context, flags } = await setup();
  const H = context.HC, workflow = H.workflows[0];
  flags.streamEOF = true;
  await H.loadTranscript(workflow);
  expect(H.transcriptFailed.has(workflow.id)).toBe(true);
  expect(H.transcripts.get(workflow.id)[0].t).toBe("hello");
  expect(H.transcripts.get(workflow.id)[1].t).toContain("before a terminal event");
  flags.streamEOF = false;
  await H.loadTranscript(workflow);
  expect(H.transcriptFailed.has(workflow.id)).toBe(false);
  expect(H.transcripts.get(workflow.id)).toHaveLength(1);
});

it("bounds polling reads without timing out long-running RPC mutations", async () => {
  let releaseMutation, slowRpc = false;
  const { context } = await setup(async (path, init) => {
    const method = path === "/rpc" ? JSON.parse(init.body).method : null;
    if (path === "/slow-read" || slowRpc && method === "gc_drafts") {
      return new Promise((resolve, reject) => init.signal.addEventListener("abort", () => reject(new Error("read timeout")), { once: true }));
    }
    if (method === "learn_rules") {
      expect(init.signal).toBeUndefined();
      return new Promise(resolve => { releaseMutation = () => resolve({ ok: true, status: 200, json: async () => ({ result: { learned: true } }) }); });
    }
  });
  const H = context.HC;
  await expect.poll(() => H.historyLoading).toBe(false);
  const alarms = new Set();
  context.setTimeout = (callback, delay) => { const alarm = { callback, delay }; alarms.add(alarm); return alarm; };
  context.clearTimeout = alarm => alarms.delete(alarm);
  const read = H.request("/slow-read");
  const readFailure = expect(read).rejects.toThrow("read timeout");
  expect([...alarms].map(alarm => alarm.delay)).toEqual([15_000]);
  [...alarms].forEach(alarm => alarm.callback());
  await readFailure;
  expect(alarms.size).toBe(0);
  const mutation = H.rpc("learn_rules", { project_root: "/tmp/repo" });
  expect(alarms.size).toBe(0);
  releaseMutation();
  expect(await mutation).toEqual({ learned: true });
  slowRpc = true;
  const rpcRead = H.rpc("gc_drafts", { project_id: null }, 15_000);
  const rpcFailure = expect(rpcRead).rejects.toThrow("read timeout");
  expect([...alarms].map(alarm => alarm.delay)).toEqual([15_000]);
  [...alarms].forEach(alarm => alarm.callback());
  await rpcFailure;
});

it("uses repository identity for memory and the canonical root for context previews", async () => {
  const { context, responses, calls, rpcCalls } = await setup();
  const H = context.HC;
  responses["/api/overview"].projects = [{ id: "alias", root: "/tmp/repo", merged_24h: 0 }];
  responses["/api/projects/owner%2Frepo/memory"] = { records: [{ id: "memory-1", kind: "lesson", payload: "Actual memory", outcome: "success", use_count: 3 }] };
  await H.refresh(true);
  expect(H.workflows[0].projectId).toBe("alias");
  expect(H.X.memory.alias[0]).toMatchObject({ text: "Actual memory", uses: 3, outcome: "success" });
  expect(calls).not.toContain("/api/projects/alias/memory");
  await H.loadContext(H.workflows[0]);
  expect(rpcCalls.find(call => call.method === "context_preview").params.request.project).toBe("/tmp/repo");
  expect(rpcCalls.find(call => call.method === "context_preview").params.request.task_profile.task_kind).toBeNull();
  H.workflows = [];
  H.history = [];
  await H.loadMemory("alias");
  expect(H.memoryUnavailable.alias).toContain("No repository identity");
});

it("shows newest events even though event_query returns oldest first", async () => {
  const { context } = await setup();
  const H = context.HC;
  const incoming = Array.from({ length: 251 }, (_, index) => ({ id: "event-" + index, ts: new Date(Date.now() - 300_000 + index * 1000).toISOString() }));
  const queries = [];
  H.rpc = async (method, params) => { queries.push({ method, params }); return incoming; };
  await H.loadEvents();
  expect(H.events).toHaveLength(200);
  expect(H.events[0].id).toBe("event-250");
  expect(queries[0].params.filters.limit).toBeUndefined();
  const lastSeen = H.events[0].ts;
  const late = { id: "late-commit", ts: new Date(new Date(lastSeen).getTime() - 1000).toISOString() };
  incoming.push(late);
  await H.loadEvents();
  expect(new Date(queries[1].params.filters.since).getTime()).toBeLessThan(new Date(late.ts).getTime());
  expect(H.events.map(event => event.id)).toContain("late-commit");
  expect(H.events).toHaveLength(200);
});


it("records operator events without secure-context-only randomUUID", async () => {
  const { context, rpcCalls } = await setup();
  const H = context.HC;
  await expect.poll(() => H.eventsLoading).toBe(false);
  context.crypto = { getRandomValues: bytes => { for (let i = 0; i < bytes.length; i++) bytes[i] = i; return bytes; } };
  await H.recordAction("retry", H.workflows[0], "Host recovered", "/api/workflows/runtime/retry", "accepted");
  const event = rpcCalls.find(call => call.method === "event_log").params.event;
  expect(event.id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  expect(event.metadata.task_id).toBe("sub-1");
  expect(event.reason).toBe("Host recovered");
});

it("retains cached events and memory when the server returns an invalid null payload", async () => {
  const { context, responses } = await setup();
  const H = context.HC;
  await expect.poll(() => H.eventsLoading).toBe(false);
  const event = { id: "keep-event", ts: new Date().toISOString() };
  H.events = [event];
  H.rpc = async () => null;
  await H.loadEvents();
  expect(H.events).toEqual([event]);
  expect(H.eventsError).toBeTruthy();
  H.X.memory["/tmp/repo"] = [{ id: "keep-memory" }];
  responses["/api/projects/owner%2Frepo/memory"] = null;
  await H.loadMemory("/tmp/repo");
  expect(H.X.memory["/tmp/repo"]).toEqual([{ id: "keep-memory" }]);
  expect(H.memoryErrors["/tmp/repo"]).toBeTruthy();
});

it("closes a pending action when its workflow disappears", () => {
  const context = {
    URLSearchParams,
    location: { search: "" },
    innerWidth: 1440,
    clearTimeout: () => {},
    setTimeout: () => 1,
    DCLogic: class {
      constructor() { this.props = {}; }
      setState(update) { this.state = { ...this.state, ...update }; }
    },
  };
  context.window = context;
  context.parent = context;
  runInNewContext(asset("harness-console-data.js"), context);
  runInNewContext(asset("harness-console-data-ext.js"), context);
  const html = asset("index.html");
  const script = html.match(/<script type="text\/x-dc"[^>]*>([\s\S]*?)<\/script>/)[1];
  runInNewContext(`${script}\nwindow.TestComponent = Component;`, context);

  const component = new context.TestComponent();
  component.state.modal = { action: "unblock", id: "completed-workflow" };
  expect(component.renderVals().hasModal).toBe(false);
  component.componentDidUpdate({});
  expect(component.state.modal).toBeNull();
  expect(component.state.toast.msg).toBe("Workflow changed");
});
