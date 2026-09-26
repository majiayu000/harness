import { readFileSync } from "node:fs";
import { join } from "node:path";
import { runInNewContext } from "node:vm";
import { expect, it, vi } from "vitest";

const asset = name => readFileSync(join(process.cwd(), "public/console-v2", name), "utf8");
const html = asset("index.html");

function setup() {
  const context = {
    URL, URLSearchParams, location: { search: "", href: "http://localhost:9800/", port: "9800" }, innerWidth: 1440,
    setTimeout: () => 1, clearTimeout: () => {},
    navigator: { clipboard: { writeText: vi.fn().mockResolvedValue() } },
    history: { replaceState: vi.fn() },
    DCLogic: class {
      constructor() { this.props = {}; }
      setState(update) { this.state = { ...this.state, ...(typeof update === "function" ? update(this.state) : update) }; }
    },
  };
  context.window = context;
  context.parent = context;
  for (const name of ["harness-console-data.js", "harness-console-data-ext.js"]) runInNewContext(asset(name), context);
  runInNewContext(html.match(/<script type="text\/x-dc"[^>]*>([\s\S]*?)<\/script>/)[1] + "\nwindow.Component = Component;", context);
  const H = context.HC;
  Object.assign(H, { loading: false, request: vi.fn().mockResolvedValue({}), recordAction: vi.fn().mockResolvedValue(), refresh: vi.fn().mockResolvedValue(), loadDetails: vi.fn(), loadContext: vi.fn(), details: new Map(), contexts: new Map(), events: [], transcriptFailed: new Set() });
  H.projects = [{ id: "project-id", root: "/tmp/project", max: 2, trend: [], dispatch: [] }];
  const workflow = { id: "wf-a", submissionId: "sub-a", projectId: "project-id", repository: "owner/repo", repo: "owner/repo", issue: 42, n: 42, ref: "owner/repo#42", title: "Fix the issue", state: "implementing", terminal: false, agent: "codex", host: "host-a", obs: "2s", age: "1m", turn: 1, max: 4, lease: "active", dependsOn: [] };
  H.workflows = [workflow];
  return { context, H, workflow, component: new context.Component() };
}

it("renders all v3 views from real state without fixture data or missing template fields", () => {
  const { component, H, workflow } = setup();
  H.events = [{ id: "event-1", ts: "2026-09-26T01:00:00Z", hook: "review", tool: "test", decision: "pass", reason: "Checks passed", metadata: { task_id: "sub-a" } }];
  workflow.approvals = [{ id: "approval-1", action: "cargo test" }, { id: "approval-2", action: "git push" }];
  workflow.inbox = { kind: "approval", actions: ["approve", "deny"], requestId: "approval-1", hint: "cargo test" };
  H.details.set("wf-a", { prompts: [{ turn: 1, phase: "implement", prompt: "Actual prompt" }], artifacts: [{ turn: 1, artifact_type: "diff", content: "Actual artifact" }] });
  H.contexts.set("wf-a", { manifest: { budget: { effective: 2000, used: 80 }, items: [{ id: "brief:task", class: "brief", decision: "included", tokens: 80 }] } });
  component.state.sel = "wf-a";
  const markup = html.split('<script type="text/x-dc"')[0];
  const aliases = new Set([...markup.matchAll(/as="(\w+)"/g)].map(match => match[1]));
  const names = new Set([...markup.matchAll(/\{\{\s*(\w+)/g)].map(match => match[1]));
  for (const screen of ["home", "fleet", "projects", "history", "worktrees", "events", "usage", "library", "system"]) {
    component.state.screen = screen;
    const values = component.renderVals();
    expect([...names].filter(name => !aliases.has(name) && !["true", "false"].includes(name) && !(name in values))).toEqual([]);
    expect(values.nav).toHaveLength(9);
    expect(values.approvals.map(row => row.command)).toEqual(["cargo test", "git push"]);
    expect(values.events[0].text).toBe("test · pass");
    expect(values.promptPreview).toEqual(["Actual prompt"]);
    expect(values.s.artifacts[0].content).toBe("Actual artifact");
    expect(values.ctxTotal).toBe("80 / 2000 tokens");
  }
  expect(html).not.toContain("harness-console-data-ext3.js");
  expect(html).not.toContain("harness-live.js");
  expect(html).not.toContain("localStorage");
});

it("handles refresh ticks and selection changes without a previous-state argument", () => {
  const { component, H, workflow } = setup();
  component.state.vw = 920;
  component.state.sel = workflow.id;
  expect(() => component.componentDidUpdate({})).not.toThrow();
  component.state.promptSel = 2;
  component.state.memFor = "project-id";
  for (let tick = 0; tick < 5; tick++) {
    expect(() => component.componentDidUpdate({})).not.toThrow();
  }
  expect(component.state.promptSel).toBe(2);
  expect(component.state.memFor).toBe("project-id");
  H.workflows.push({ ...workflow, id: "wf-b", submissionId: "sub-b" });
  component.state.sel = "wf-b";
  expect(() => component.componentDidUpdate()).not.toThrow();
  expect(component.state.promptSel).toBe(0);
  expect(H.loadDetails).toHaveBeenLastCalledWith(expect.objectContaining({ id: "wf-b" }));
});

it("confirms and records the exact approval request rather than the first request", async () => {
  const { component, H, workflow } = setup();
  workflow.approvals = [{ id: "approval-1", action: "cargo test" }, { id: "approval-2", action: "git push" }];
  workflow.inbox = { kind: "approval", actions: ["approve", "deny"], requestId: "approval-1", hint: "cargo test" };
  component.renderVals().approvals[1].approve();
  expect(H.request).not.toHaveBeenCalled();
  await component.doConfirm();
  expect(H.request).toHaveBeenCalledWith("/api/workflows/runtime/turns/sub-a/approvals/approval-2", expect.objectContaining({ body: '{"decision":"accept"}' }));
  expect(H.recordAction).toHaveBeenCalledWith("approve", expect.objectContaining({ id: "wf-a" }), "", "/api/workflows/runtime/turns/sub-a/approvals/approval-2", "accepted");
});

it("does not execute an approval removed since the confirmation opened", async () => {
  const { component, H, workflow } = setup();
  workflow.approvals = [{ id: "approval-1", action: "cargo test" }];
  workflow.inbox = { kind: "approval", actions: ["approve"], requestId: "approval-1" };
  component.renderVals().approvals[0].approve();
  workflow.approvals = [];
  await component.doConfirm();
  expect(H.request).not.toHaveBeenCalled();
  expect(component.state.modal).toBeNull();
});

it("requires a bulk reason, skips ineligible rows and preserves partial failures", async () => {
  const { component, H, workflow } = setup();
  const failed = { ...workflow, state: "failed", inbox: { kind: "failed", actions: ["retry"] } };
  H.workflows = [failed, { ...failed, id: "wf-b", submissionId: "sub-b", ref: "owner/repo#43" }, { ...failed, id: "wf-c", inbox: { kind: "failed", actions: ["cancel"] } }];
  component.state.picked = { "wf-a": true, "wf-b": true, "wf-c": true };
  component.bulk("retry");
  expect(component.state.modal.skipped).toBe(1);
  expect(component.renderVals().m.items).toHaveLength(2);
  await component.doConfirm();
  expect(H.request).not.toHaveBeenCalled();
  component.state.reason = "Runtime host recovered";
  H.request.mockResolvedValueOnce({}).mockRejectedValueOnce(new Error("not retryable"));
  await component.doConfirm();
  expect(H.request).toHaveBeenCalledTimes(2);
  expect(JSON.parse(H.request.mock.calls[0][1].body)).toEqual({ workflow_id: "wf-a", reason: "Runtime host recovered" });
  expect(H.recordAction.mock.calls.map(call => call[4])).toEqual(["accepted", "failed"]);
  expect(component.state.toast.msg).toBe("1/2 requests accepted");
  expect(component.state.toast.sub).toContain("not retryable");
});

it("does not repeat a successful mutation when action logging fails", async () => {
  const { component, H, workflow } = setup();
  component.openModal("cancel", workflow);
  H.recordAction.mockRejectedValue(new Error("event store offline"));
  await component.doConfirm();
  expect(H.request).toHaveBeenCalledTimes(1);
  expect(component.state.toast.msg).toBe("1/1 requests accepted");
  expect(component.state.toast.sub).toContain("event store offline");
});

it("keeps dependency cycles finite and quotes cleanup paths without forcing deletion", async () => {
  const { component, H, workflow, context } = setup();
  H.workflows = [{ ...workflow, dependsOn: ["sub-b"] }, { ...workflow, id: "wf-b", submissionId: "sub-b", dependsOn: ["sub-a"] }];
  expect(component.renderVals().treeRows).toHaveLength(2);
  const command = component.cleanupCommand({ source_repo: "/tmp/a b", workspace_path: "/tmp/it's here" });
  expect(command).toBe("git -C '/tmp/a b' worktree remove '/tmp/it'\\''s here'");
  expect(command).not.toContain("--force");
  await component.copy(command, "Cleanup copied");
  expect(context.navigator.clipboard.writeText).toHaveBeenCalledWith(command);
  expect(H.request).not.toHaveBeenCalled();
});

it("shows partial errors and empty onboarding without inventing history or usage", () => {
  const { component, H } = setup();
  H.workflows = [];
  H.projects = [];
  expect(component.renderVals().isOnboard).toBe(true);
  expect(component.renderVals().recEmptyLabel).toContain("Run a dry run");
  expect(html).toContain("{{ recEmptyLabel }}");
  expect(html).toContain("{{ inboxEmptyLabel }}");
  H.loadError = "approvals unavailable";
  expect(component.renderVals().isOnboard).toBe(false);
  expect(component.renderVals().banner.sub).toContain("approvals unavailable");
  expect(component.renderVals().inboxEmptyLabel).toBe("Inbox data unavailable.");
  expect(component.renderVals().usageStats[0].v).toBe("—");
  component.state.vw = 390;
  const mobile = component.renderVals();
  expect(mobile.isMobile).toBe(true);
  expect(mobile.isHome).toBe(false);
  expect(mobile.cols).toBe("minmax(0,1fr)");
});
