import { describe, expect, it, vi, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { useTaskDetail, useTaskPrompts, useWorktrees } from "./queries";
import { TOKEN_KEY } from "./api";

// ── helpers ───────────────────────────────────────────────────────────────────

function makeWrapper() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const Wrapper: React.FC<{ children: React.ReactNode }> = ({ children }) =>
    React.createElement(QueryClientProvider, { client: qc }, children);
  return Wrapper;
}

type TaskStub = { id: string; status: string; turn?: number; project?: null };

function mockFetch(tasks: TaskStub[]) {
  const overview = { projects: [], runtimes: [], kpi: { active_tasks: 0 } };
  global.fetch = vi.fn().mockImplementation((url: string) => {
    const body = url.includes("/api/overview")
      ? JSON.stringify(overview)
      : JSON.stringify(tasks);
    return Promise.resolve(new Response(body, { status: 200 }));
  }) as unknown as typeof fetch;
}

const originalFetch = global.fetch;
afterEach(() => {
  global.fetch = originalFetch;
  vi.restoreAllMocks();
  sessionStorage.clear();
});

// ── useWorktrees: active-status filtering ─────────────────────────────────────

describe("useWorktrees – active-status filtering", () => {
  it("includes all non-terminal statuses as active worktrees", async () => {
    mockFetch([
      { id: "1", status: "implementing", turn: 1, project: null },
      { id: "2", status: "pending", turn: 0, project: null },
      { id: "3", status: "agent_review", turn: 3, project: null },
      { id: "4", status: "waiting", turn: 2, project: null },
      { id: "5", status: "reviewing", turn: 4, project: null },
      { id: "6", status: "awaiting_deps", turn: 0, project: null },
    ]);
    const { result } = renderHook(() => useWorktrees(), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.cards).toHaveLength(6);
  });

  it("excludes terminal statuses: done, failed, cancelled", async () => {
    mockFetch([
      { id: "1", status: "done", turn: 5, project: null },
      { id: "2", status: "failed", turn: 2, project: null },
      { id: "3", status: "cancelled", turn: 1, project: null },
      { id: "4", status: "implementing", turn: 3, project: null },
    ]);
    const { result } = renderHook(() => useWorktrees(), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.cards).toHaveLength(1);
    expect(result.current.cards[0].taskId).toBe("4");
  });

  it("returns empty array when all tasks are terminal", async () => {
    mockFetch([
      { id: "a", status: "done", turn: 1, project: null },
      { id: "b", status: "failed", turn: 0, project: null },
    ]);
    const { result } = renderHook(() => useWorktrees(), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.cards).toHaveLength(0);
  });
});

// ── stream URL construction ───────────────────────────────────────────────────
// Mirrors the logic in openStream() in Worktrees.tsx to prevent regressions.

function buildStreamUrl(taskId: string): string {
  const tok = (globalThis.sessionStorage?.getItem?.(TOKEN_KEY) ?? "").trim();
  const base = `/tasks/${taskId}/stream`;
  return tok ? `${base}?token=${encodeURIComponent(tok)}` : base;
}

describe("stream URL construction", () => {
  it("uses /tasks/{id}/stream path (no /api prefix)", () => {
    expect(buildStreamUrl("abc-123")).toBe("/tasks/abc-123/stream");
  });

  it("appends token query param when session token is set", () => {
    sessionStorage.setItem(TOKEN_KEY, "mytoken");
    expect(buildStreamUrl("abc-123")).toBe("/tasks/abc-123/stream?token=mytoken");
  });

  it("omits token param when no session token", () => {
    expect(buildStreamUrl("abc-123")).not.toContain("token=");
  });
});

describe("task detail queries", () => {
  it("does not fetch detail or prompts when no task id is provided", async () => {
    global.fetch = vi.fn() as unknown as typeof fetch;

    const { result: detailResult } = renderHook(() => useTaskDetail(null), { wrapper: makeWrapper() });
    const { result: promptsResult } = renderHook(() => useTaskPrompts(null), { wrapper: makeWrapper() });

    await waitFor(() => {
      expect(detailResult.current.fetchStatus).toBe("idle");
      expect(promptsResult.current.fetchStatus).toBe("idle");
    });
    expect(global.fetch).not.toHaveBeenCalled();
  });

  it("fetches task detail from /tasks/{id}", async () => {
    global.fetch = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({
          id: "task-123",
          status: "running",
          turn: 2,
          pr_url: null,
          error: null,
          source: "dashboard",
          parent_id: null,
          repo: "majiayu000/harness",
          description: "Task detail",
          created_at: "2026-04-22T13:00:00Z",
          phase: "implement",
          depends_on: [],
          subtask_ids: [],
          project: "/tmp/harness",
          rounds: [],
        }),
        { status: 200 },
      ),
    ) as unknown as typeof fetch;

    const { result } = renderHook(() => useTaskDetail("task-123"), { wrapper: makeWrapper() });

    await waitFor(() => expect(result.current.data?.id).toBe("task-123"));
    expect(global.fetch).toHaveBeenCalledWith(
      "/tasks/task-123",
      expect.objectContaining({
        headers: { Accept: "application/json" },
      }),
    );
  });

  it("fetches prompts from /tasks/{id}/prompts and sorts them chronologically", async () => {
    global.fetch = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify([
          {
            task_id: "task-123",
            turn: 2,
            phase: "implement",
            prompt: "retry prompt",
            created_at: "2026-04-22T13:00:02Z",
          },
          {
            task_id: "task-123",
            turn: 1,
            phase: "plan",
            prompt: "planner prompt",
            created_at: "2026-04-22T13:00:01Z",
          },
        ]),
        { status: 200 },
      ),
    ) as unknown as typeof fetch;

    const { result } = renderHook(() => useTaskPrompts("task-123"), { wrapper: makeWrapper() });

    await waitFor(() => expect(result.current.data).toHaveLength(2));
    expect(result.current.data?.map((prompt) => prompt.turn)).toEqual([1, 2]);
    expect(global.fetch).toHaveBeenCalledWith(
      "/tasks/task-123/prompts",
      expect.objectContaining({
        headers: { Accept: "application/json" },
      }),
    );
  });
});
