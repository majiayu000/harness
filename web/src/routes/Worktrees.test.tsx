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

function worktreeCard(overrides: Partial<import("@/lib/queries").WorktreeCard> = {}) {
  return {
    taskId: "runtime-task-1",
    runtimeWorkflowId: null,
    workspacePath: "/var/harness/workspaces/runtime-task-1",
    pathShort: "workspaces/runtime-task-1",
    sourceRepo: "/Users/example/src/repo",
    repo: "owner/repo",
    branch: "harness/runtime-task-1",
    status: "implementing",
    phase: "implement",
    description: "Fix worktree cards",
    turn: 1,
    maxTurns: null,
    createdAt: "2026-04-21T03:40:21Z",
    durationSecs: 125,
    prUrl: null,
    project: "/Users/example/src/repo",
    ...overrides,
  };
}

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
        worktreeCard({
          taskId: "runtime-task-1",
          runtimeWorkflowId: "runtime-workflow-1",
        }),
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
        worktreeCard({
          taskId: "runtime-task-2",
          runtimeWorkflowId: "runtime-workflow-2",
        }),
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
      expect(invalidateQueries).toHaveBeenCalledWith({ queryKey: ["worktrees"] });
      expect(invalidateQueries).toHaveBeenCalledWith({ queryKey: ["tasks"] });
      expect(invalidateQueries).toHaveBeenCalledWith({ queryKey: ["workflow-runtime-tree"] });
    });
  });

  it("renders workspace metadata without placeholder resource metrics", () => {
    mockUseWorktrees.mockReturnValue({
      cards: [
        worktreeCard({
          branch: "harness/1234567890abcdef",
          durationSecs: 3_900,
          maxTurns: 10,
          turn: 4,
          prUrl: "https://github.com/owner/repo/pull/123",
          phase: "review",
        }),
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

    wrap(<Worktrees />);

    expect(screen.getByText("harness/runtime-")).toBeInTheDocument();
    expect(screen.getByText("1h")).toBeInTheDocument();
    expect(screen.getByText("Fix worktree cards")).toBeInTheDocument();
    expect(screen.getByText("review")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "PR" })).toHaveAttribute(
      "href",
      "https://github.com/owner/repo/pull/123",
    );
    expect(screen.queryByText("CPU")).not.toBeInTheDocument();
    expect(screen.queryByText("RAM")).not.toBeInTheDocument();
    expect(screen.queryByText("Disk")).not.toBeInTheDocument();
  });

  it("links the empty state CTA to dashboard submission", () => {
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

    expect(screen.getByRole("link", { name: "Submit task" })).toHaveAttribute(
      "href",
      "/dashboard?tab=submit",
    );
  });
});
