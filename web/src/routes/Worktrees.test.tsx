import { describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { PaletteProvider } from "@/lib/palette";
import { DOCS_URL } from "@/lib/links";
import { Worktrees } from "./Worktrees";

vi.mock("@/lib/queries", () => ({
  useWorktrees: vi.fn(),
  useOverview: vi.fn(),
}));

vi.mock("@/lib/api", () => ({
  apiFetch: vi.fn(),
  TOKEN_KEY: "harness_token",
}));

import { useOverview, useWorktrees } from "@/lib/queries";
import { apiFetch } from "@/lib/api";

const mockUseWorktrees = useWorktrees as ReturnType<typeof vi.fn>;
const mockUseOverview = useOverview as ReturnType<typeof vi.fn>;
const mockApiFetch = apiFetch as ReturnType<typeof vi.fn>;

function makeQueryClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

function wrap(ui: React.ReactElement, qc = makeQueryClient()) {
  return render(
    <QueryClientProvider client={qc}>
      <PaletteProvider>
        <MemoryRouter>{ui}</MemoryRouter>
      </PaletteProvider>
    </QueryClientProvider>,
  );
}

describe("<Worktrees>", () => {
  it("links Docs to the repository documentation", () => {
    mockUseWorktrees.mockReturnValue({
      cards: [],
      isLoading: false,
      error: null,
    });
    mockUseOverview.mockReturnValue({
      data: {
        projects: [],
        runtimes: [],
        kpi: {
          active_tasks: 0,
        },
      },
    });

    wrap(<Worktrees />);

    expect(screen.getByRole("link", { name: "Docs" })).toHaveAttribute("href", DOCS_URL);
  });

  it("cancels runtime worktrees through the workflow runtime endpoint", async () => {
    mockUseWorktrees.mockReturnValue({
      cards: [
        {
          taskId: "runtime-task-1",
          runtimeWorkflowId: "runtime-workflow-1",
          pathShort: "repo/project",
          branch: "—",
          status: "implementing",
          turn: 1,
          maxTurns: null,
          cpuPct: null,
          ramPct: null,
          diskBytes: null,
        },
      ],
      isLoading: false,
      error: null,
    });
    mockUseOverview.mockReturnValue({
      data: {
        projects: [],
        runtimes: [],
        kpi: {
          active_tasks: 1,
        },
      },
    });
    mockApiFetch.mockResolvedValue(new Response("{}", { status: 200 }));

    wrap(<Worktrees />);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    await waitFor(() => {
      expect(mockApiFetch).toHaveBeenCalledWith("/api/workflows/runtime/cancel", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ workflow_id: "runtime-workflow-1" }),
      });
    });
  });

  it("refreshes worktree data when runtime cancellation fails", async () => {
    const qc = makeQueryClient();
    const invalidateQueries = vi.spyOn(qc, "invalidateQueries");
    mockUseWorktrees.mockReturnValue({
      cards: [
        {
          taskId: "runtime-task-2",
          runtimeWorkflowId: "runtime-workflow-2",
          pathShort: "repo/project",
          branch: "—",
          status: "implementing",
          turn: 1,
          maxTurns: null,
          cpuPct: null,
          ramPct: null,
          diskBytes: null,
        },
      ],
      isLoading: false,
      error: null,
    });
    mockUseOverview.mockReturnValue({
      data: {
        projects: [],
        runtimes: [],
        kpi: {
          active_tasks: 1,
        },
      },
    });
    mockApiFetch.mockRejectedValueOnce(new Error("workflow already terminal"));

    wrap(<Worktrees />, qc);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent("workflow already terminal");
      expect(invalidateQueries).toHaveBeenCalledWith({ queryKey: ["tasks"] });
      expect(invalidateQueries).toHaveBeenCalledWith({ queryKey: ["workflow-runtime-tree"] });
    });
  });
});
