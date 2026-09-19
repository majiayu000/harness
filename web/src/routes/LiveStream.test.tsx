import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { PaletteProvider } from "@/lib/palette";
import { TOKEN_KEY } from "@/lib/api";
import { Worktrees } from "./Worktrees";
import { SubmitSuccess } from "./dashboard/SubmitSuccess";

vi.mock("@/lib/queries", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/queries")>()),
  useOverview: () => ({ data: { projects: [], runtimes: [], kpi: { active_tasks: 1 } } }),
  useTaskDetail: () => ({ data: undefined, isLoading: false, isError: false }),
  useWorktrees: () => ({
    cards: [{
      taskId: "task-1", runtimeSubmissionId: "submission::/one", runtimeWorkflowId: "workflow-1",
      branch: "fix/test", status: "implementing", phase: "implement", durationSecs: 1,
      workspacePath: "/workspace", pathShort: "workspace", description: "Test stream", turn: 1,
    }],
    isLoading: false,
    error: null,
  }),
}));

afterEach(() => {
  sessionStorage.removeItem(TOKEN_KEY);
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function authenticatedStream() {
  sessionStorage.setItem(TOKEN_KEY, "test-stream-token");
  const fetch = vi.fn().mockImplementation(async (_url: string, init: RequestInit) => {
    if (new Headers(init.headers).get("Authorization") !== "Bearer test-stream-token") {
      return new Response("unauthorized", { status: 401 });
    }
    return new Response('data: {"type":"message_delta","text":"Authenticated output"}\n\ndata: {"type":"done"}\n\n');
  });
  vi.stubGlobal("fetch", fetch);
  const open = vi.spyOn(window, "open").mockImplementation(() => null);
  return { fetch, open };
}

function wrap(ui: React.ReactElement) {
  return render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <PaletteProvider><MemoryRouter>{ui}</MemoryRouter></PaletteProvider>
    </QueryClientProvider>,
  );
}

describe("authenticated live output actions", () => {
  it("opens worktree logs in the authenticated submission viewer", async () => {
    const { fetch, open } = authenticatedStream();
    wrap(<Worktrees />);
    expect(fetch).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Logs" }));
    fireEvent.click(screen.getByRole("button", { name: "output" }));
    expect(await screen.findByText("Authenticated output")).toBeVisible();
    expect(fetch.mock.calls[0][0]).toBe("/api/workflows/runtime/submissions/submission%3A%3A%2Fone/stream");
    expect(open).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(fetch.mock.calls[0][1].signal.aborted).toBe(true);
  });

  it("focuses the existing authenticated stream when watching a submission", async () => {
    const { fetch, open } = authenticatedStream();
    wrap(<SubmitSuccess taskId="submission::/one" executionPath="workflow_runtime" onReset={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "Watch live" }));
    expect(screen.getByLabelText("Live output")).toHaveFocus();
    await waitFor(() => expect(screen.getByLabelText("Live output")).toHaveTextContent("Authenticated output"));
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(fetch.mock.calls[0][0]).toBe("/api/workflows/runtime/submissions/submission%3A%3A%2Fone/stream");
    expect(open).not.toHaveBeenCalled();
  });
});
