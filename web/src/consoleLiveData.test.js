import { readFileSync } from "node:fs";
import { join } from "node:path";
import { runInNewContext } from "node:vm";
import { expect, it } from "vitest";

const asset = (name) => readFileSync(join(process.cwd(), "public/console-v2", name), "utf8");

it("keeps the current invocation, hourly series, and snake-case stream events", async () => {
  let readerCancelled = false;
  const task = {
    id: "sub-1", workflow: { id: "wf-1", state: "implementing" }, repo: "owner/repo",
    external_id: "42", description: "Fix issue 42", status: "implementing", turn: 2,
  };
  const responses = {
    "/api/workflows/runtime/submissions?limit=200": { data: [task], page: { has_more: false } },
    "/api/operator-monitor": { health: { status: "ok", degraded_subsystems: [], uptime_secs: 30 }, failures: [], operator_actions: [], activity: {} },
    "/api/overview": { projects: [], runtimes: [] },
    "/api/usage-monitor": {
      summary: { total_tokens: 100, cache_read_input_tokens: 0, request_count: 1 },
      agent_invocations: [
        { workflow_id: "wf-1", agent_runtime: "current-agent", lease_state: "active_leased", activity: "implement_issue" },
        { workflow_id: "wf-1", agent_runtime: "old-agent", lease_state: "released", activity: "plan_issue" },
      ],
    },
    "/api/dashboard": { global: { max_concurrent: 2 }, runtime_hosts: [] },
    "/api/operator-snapshot": {},
    "/api/worktrees": [],
    "/api/intake": { channels: [] },
    "/projects": [],
    "/api/projects/repo/memory": { records: [] },
    "/api/token-usage": { by_hour: { "2026-09-25T12": { input_tokens: 100 } } },
  };
  const context = {
    URLSearchParams, TextDecoder, console,
    sessionStorage: { getItem: () => null },
    location: { origin: "http://localhost" },
    addEventListener: () => {},
    setInterval: () => 1,
    fetch: async (path) => {
      if (path.endsWith("/stream")) {
        const data = new TextEncoder().encode('data: {"type":"message_delta","text":"hello"}\n\ndata: {"type":"done"}\n\n');
        let sent = false;
        return { ok: true, body: { getReader: () => ({
          read: async () => sent ? { done: true } : ((sent = true), { done: false, value: data }),
          cancel: async () => { readerCancelled = true; },
        }) } };
      }
      if (path === "/rpc") return { ok: true, status: 200, json: async () => ({ result: [] }) };
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

  expect(context.HC.workflows[0].agent).toBe("current-agent");
  expect(context.HC.workflows[0].lease).toBe("active");
  expect(context.HC.X.usage.hourly).toEqual([0.0001]);

  await context.HC.refresh();
  expect(context.HC.X.usage.hourly).toEqual([0.0001]);

  await context.HC.loadTranscript(context.HC.workflows[0]);
  expect(context.HC.transcripts.get("wf-1")).toEqual([{ t: "hello", c: "oklch(0.86 0.005 275)" }]);
  expect(readerCancelled).toBe(true);
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
