import { describe, expect, it, vi, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { useAllTasks, useTasks, useWorktrees, useTaskDetail, useTaskStream } from "./queries";
import { TOKEN_KEY } from "./api";

// ── helpers ───────────────────────────────────────────────────────────────────

function makeWrapper() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const Wrapper: React.FC<{ children: React.ReactNode }> = ({ children }) =>
    React.createElement(QueryClientProvider, { client: qc }, children);
  return Wrapper;
}

function worktreeEntry(overrides: Record<string, unknown> = {}) {
  return {
    task_id: "task-1",
    branch: "harness/task-1",
    workspace_path: "/var/harness/workspaces/task-1",
    path_short: "workspaces/task-1",
    source_repo: "/Users/example/src/repo",
    repo: "owner/repo",
    runtime_workflow_id: null,
    runtime_submission_id: null,
    status: "implementing",
    phase: "implement",
    description: "Fix worktree cards",
    turn: 1,
    max_turns: null,
    created_at: "2026-04-21T03:40:21Z",
    duration_secs: 60,
    pr_url: null,
    project: "/Users/example/src/repo",
    ...overrides,
  };
}

function mockWorktreeFetch(worktrees: Array<ReturnType<typeof worktreeEntry>>) {
  global.fetch = vi.fn().mockImplementation((url: string) => {
    if (url === "/api/worktrees") {
      return Promise.resolve(new Response(JSON.stringify(worktrees), { status: 200 }));
    }
    return Promise.resolve(new Response("{}", { status: 404 }));
  }) as unknown as typeof fetch;
}

const originalFetch = global.fetch;
afterEach(() => {
  global.fetch = originalFetch;
  vi.restoreAllMocks();
  sessionStorage.clear();
});

// ── useWorktrees: API-backed cards ────────────────────────────────────────────

describe("useWorktrees", () => {
  it("fetches worktrees from /api/worktrees", async () => {
    mockWorktreeFetch([worktreeEntry({ task_id: "task-1", duration_secs: 125 })]);
    const { result } = renderHook(() => useWorktrees(), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.cards).toHaveLength(1);
    expect(result.current.cards[0].taskId).toBe("task-1");
  });

  it("maps API fields to card fields", async () => {
    mockWorktreeFetch([
      worktreeEntry({
        task_id: "runtime-task-1",
        branch: "harness/runtime-task-1",
        workspace_path: "/var/harness/workspaces/runtime-task-1",
        path_short: "workspaces/runtime-task-1",
        source_repo: "/Users/example/src/repo",
        runtime_workflow_id: "workflow-1",
        runtime_submission_id: "submission-1",
        status: "agent_review",
        phase: "review",
        description: "Review generated PR",
        turn: 3,
        max_turns: 10,
        duration_secs: 300,
        pr_url: "https://github.com/owner/repo/pull/123",
      }),
    ]);
    const { result } = renderHook(() => useWorktrees(), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.cards[0]).toMatchObject({
      taskId: "runtime-task-1",
      workspacePath: "/var/harness/workspaces/runtime-task-1",
      pathShort: "workspaces/runtime-task-1",
      sourceRepo: "/Users/example/src/repo",
      runtimeWorkflowId: "workflow-1",
      runtimeSubmissionId: "submission-1",
      branch: "harness/runtime-task-1",
      status: "agent_review",
      phase: "review",
      description: "Review generated PR",
      turn: 3,
      maxTurns: 10,
      durationSecs: 300,
      prUrl: "https://github.com/owner/repo/pull/123",
    });
  });

  it("normalizes zero max_turns to an unknown turn budget", async () => {
    mockWorktreeFetch([worktreeEntry({ max_turns: 0 })]);
    const { result } = renderHook(() => useWorktrees(), { wrapper: makeWrapper() });

    await waitFor(() => expect(result.current.isLoading).toBe(false));

    expect(result.current.cards[0].maxTurns).toBeNull();
  });

  it("sorts failed, review, implementing, then queued by duration", async () => {
    mockWorktreeFetch([
      worktreeEntry({ task_id: "queued", status: "pending", duration_secs: 999 }),
      worktreeEntry({ task_id: "implementing-short", status: "implementing", duration_secs: 5 }),
      worktreeEntry({ task_id: "review", status: "reviewing", duration_secs: 10 }),
      worktreeEntry({ task_id: "implementing-long", status: "implementing", duration_secs: 50 }),
      worktreeEntry({ task_id: "failed", status: "failed", duration_secs: 1 }),
    ]);
    const { result } = renderHook(() => useWorktrees(), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.cards.map((card) => card.taskId)).toEqual([
      "failed",
      "review",
      "implementing-long",
      "implementing-short",
      "queued",
    ]);
  });
});

// ── stream URL construction ───────────────────────────────────────────────────
// Mirrors the logic in openStream() in Worktrees.tsx to prevent regressions.

function buildStreamUrl(taskId: string): string {
  const tok = (globalThis.sessionStorage?.getItem?.(TOKEN_KEY) ?? "").trim();
  const base = `/api/workflows/runtime/submissions/${taskId}/stream`;
  return tok ? `${base}?token=${encodeURIComponent(tok)}` : base;
}

describe("stream URL construction", () => {
  it("uses the workflow runtime submission stream path", () => {
    expect(buildStreamUrl("abc-123")).toBe(
      "/api/workflows/runtime/submissions/abc-123/stream",
    );
  });

  it("appends token query param when session token is set", () => {
    sessionStorage.setItem(TOKEN_KEY, "mytoken");
    expect(buildStreamUrl("abc-123")).toBe(
      "/api/workflows/runtime/submissions/abc-123/stream?token=mytoken",
    );
  });

  it("omits token param when no session token", () => {
    expect(buildStreamUrl("abc-123")).not.toContain("token=");
  });
});

// ── useTasks ─────────────────────────────────────────────────────────────────

describe("useTasks", () => {
  it("fetches the active workflow runtime submission list", async () => {
    const task = { id: "t1", status: "implementing", turn: 1, project: null };
    const taskList = {
      data: [task],
      page: { limit: 200, has_more: false, next_cursor: null },
      counts: {
        total: 1,
        running: 1,
        failed: 0,
        by_status: { implementing: 1 },
        by_scheduler_state: {},
      },
    };
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(JSON.stringify(taskList), { status: 200 }),
    );
    global.fetch = fetchMock as unknown as typeof fetch;

    const { result } = renderHook(() => useTasks(), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isLoading).toBe(false));

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/workflows/runtime/submissions?active=true&limit=200",
      expect.objectContaining({
        headers: expect.objectContaining({ Accept: "application/json" }),
      }),
    );
    expect(result.current.data?.data).toMatchObject([{ id: "t1", status: "implementing" }]);
  });
});

describe("useAllTasks", () => {
  it("follows task list cursors and returns the combined rows", async () => {
    const firstPage = {
      data: [{ id: "first", status: "done", turn: 1, project: null }],
      page: { limit: 1, has_more: true, next_cursor: "cursor-2" },
      counts: {
        total: 2,
        running: 0,
        failed: 0,
        by_status: { done: 2 },
        by_scheduler_state: {},
      },
    };
    const secondPage = {
      data: [{ id: "second", status: "done", turn: 1, project: null }],
      page: { limit: 1, has_more: false, next_cursor: null },
      counts: firstPage.counts,
    };
    const fetchMock = vi.fn().mockImplementation((url: string) => {
      if (url === "/api/workflows/runtime/submissions?status=done&limit=1") {
        return Promise.resolve(new Response(JSON.stringify(firstPage), { status: 200 }));
      }
      if (url === "/api/workflows/runtime/submissions?status=done&limit=1&cursor=cursor-2") {
        return Promise.resolve(new Response(JSON.stringify(secondPage), { status: 200 }));
      }
      return Promise.resolve(new Response("not found", { status: 404 }));
    });
    global.fetch = fetchMock as unknown as typeof fetch;

    const { result } = renderHook(() => useAllTasks({ status: "done", limit: 1 }), {
      wrapper: makeWrapper(),
    });
    await waitFor(() => expect(result.current.isLoading).toBe(false));

    expect(result.current.data?.data.map((task) => task.id)).toEqual(["first", "second"]);
    expect(result.current.data?.page.has_more).toBe(false);
    expect(result.current.data?.page.next_cursor).toBeNull();
  });
});

// ── useTaskDetail ─────────────────────────────────────────────────────────────

describe("useTaskDetail", () => {
  it("fetches a workflow runtime submission detail", async () => {
    const task = { id: "t1", status: "implementing", task_kind: "issue", rounds: [] };
    global.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify(task), { status: 200 }),
    ) as unknown as typeof fetch;

    const { result } = renderHook(() => useTaskDetail("t1"), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.data).toMatchObject({ id: "t1", status: "implementing" });
  });

  it("is disabled when id is null", () => {
    global.fetch = vi.fn() as unknown as typeof fetch;
    const { result } = renderHook(() => useTaskDetail(null), { wrapper: makeWrapper() });
    expect(result.current.fetchStatus).toBe("idle");
    expect(global.fetch).not.toHaveBeenCalled();
  });

  it("exposes error when the server returns 404", async () => {
    global.fetch = vi.fn().mockResolvedValue(
      new Response("not found", { status: 404 }),
    ) as unknown as typeof fetch;

    const { result } = renderHook(() => useTaskDetail("missing"), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isError).toBe(true));
    expect(result.current.error).toBeTruthy();
  });
});

// ── useTaskStream ─────────────────────────────────────────────────────────────

describe("useTaskStream", () => {
  it("calls onChunk for each MessageDelta event", async () => {
    const chunks: string[] = [];
    const encoder = new TextEncoder();
    const stream = new ReadableStream({
      start(controller) {
        controller.enqueue(encoder.encode('data: {"type":"MessageDelta","text":"hello "}\n\n'));
        controller.enqueue(encoder.encode('data: {"type":"MessageDelta","text":"world"}\n\n'));
        controller.enqueue(encoder.encode('data: {"type":"Done"}\n\n'));
        controller.close();
      },
    });
    global.fetch = vi.fn().mockResolvedValue(
      new Response(stream, { status: 200 }),
    ) as unknown as typeof fetch;

    renderHook(() => useTaskStream("t1", (text) => chunks.push(text)), {
      wrapper: makeWrapper(),
    });

    await waitFor(() => expect(chunks.length).toBe(2));
    expect(chunks).toEqual(["hello ", "world"]);
  });

  it("aborts the fetch when the hook cleans up", async () => {
    const abortSpy = vi.fn();
    const realAbortController = globalThis.AbortController;
    globalThis.AbortController = class {
      signal = { aborted: false } as AbortSignal;
      abort = abortSpy;
    } as unknown as typeof AbortController;

    const stream = new ReadableStream({ start() {} });
    global.fetch = vi.fn().mockResolvedValue(
      new Response(stream, { status: 200 }),
    ) as unknown as typeof fetch;

    const { unmount } = renderHook(() => useTaskStream("t1", vi.fn()), {
      wrapper: makeWrapper(),
    });

    unmount();
    expect(abortSpy).toHaveBeenCalled();

    globalThis.AbortController = realAbortController;
  });
});
