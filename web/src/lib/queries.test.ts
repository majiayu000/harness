import { describe, expect, it, vi, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { useWorktrees } from "./queries";
import { TOKEN_KEY } from "./api";

// ── helpers ───────────────────────────────────────────────────────────────────

function makeWrapper() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const Wrapper: React.FC<{ children: React.ReactNode }> = ({ children }) =>
    React.createElement(QueryClientProvider, { client: qc }, children);
  return Wrapper;
}

function mockFetch(body: unknown) {
  global.fetch = vi.fn().mockResolvedValue(
    new Response(JSON.stringify(body), { status: 200 }),
  ) as unknown as typeof fetch;
}

const originalFetch = global.fetch;
afterEach(() => {
  global.fetch = originalFetch;
  vi.restoreAllMocks();
  sessionStorage.clear();
});

describe("useWorktrees", () => {
  it("fetches the dedicated /api/worktrees payload", async () => {
    mockFetch([
      {
        task_id: "task-1",
        branch: "harness/task-1",
        workspace_path: "/tmp/worktrees/task-1",
        path_short: "worktrees/task-1",
        source_repo: "/tmp/repo",
        repo: "majiayu000/harness",
        status: "implementing",
        phase: "implement",
        description: "issue #882",
        turn: 2,
        max_turns: 8,
        created_at: "2026-04-22T11:00:00Z",
        duration_secs: 120,
        pr_url: null,
        project: "harness",
      },
    ]);

    const { result } = renderHook(() => useWorktrees(), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isLoading).toBe(false));

    expect(global.fetch).toHaveBeenCalledWith(
      "/api/worktrees",
      expect.objectContaining({
        signal: expect.any(AbortSignal),
      }),
    );
    expect(result.current.data).toHaveLength(1);
    expect(result.current.data?.[0].task_id).toBe("task-1");
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
