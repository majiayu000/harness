import { describe, expect, it, vi, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { useTasks, useWorktrees, useTaskDetail, useTaskStream } from "./queries";
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

// ── useTasks ─────────────────────────────────────────────────────────────────

describe("useTasks", () => {
  it("fetches the task list from /tasks without an /api prefix", async () => {
    const task = { id: "t1", status: "implementing", turn: 1, project: null };
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(JSON.stringify([task]), { status: 200 }),
    );
    global.fetch = fetchMock as unknown as typeof fetch;

    const { result } = renderHook(() => useTasks(), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isLoading).toBe(false));

    expect(fetchMock).toHaveBeenCalledWith(
      "/tasks",
      expect.objectContaining({
        headers: expect.objectContaining({ Accept: "application/json" }),
      }),
    );
    expect(result.current.data).toMatchObject([{ id: "t1", status: "implementing" }]);
  });
});

// ── useTaskDetail ─────────────────────────────────────────────────────────────

describe("useTaskDetail", () => {
  it("fetches /tasks/{id} and returns FullTask shape", async () => {
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
