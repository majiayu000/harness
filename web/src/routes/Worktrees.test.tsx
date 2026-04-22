import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PaletteProvider } from "@/lib/palette";
import { Worktrees } from "./Worktrees";

vi.mock("@/lib/queries", () => ({
  useOverview: vi.fn(),
  useWorktrees: vi.fn(),
}));

vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    apiFetch: vi.fn(),
  };
});

import { useOverview, useWorktrees } from "@/lib/queries";

const mockUseOverview = useOverview as ReturnType<typeof vi.fn>;
const mockUseWorktrees = useWorktrees as ReturnType<typeof vi.fn>;

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <PaletteProvider>
        <MemoryRouter>
          <Worktrees />
        </MemoryRouter>
      </PaletteProvider>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  mockUseOverview.mockReturnValue({
    data: {
      projects: [],
      runtimes: [],
      kpi: { active_tasks: 0 },
    },
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("<Worktrees>", () => {
  it("sorts failed worktrees ahead of reviewing and implementing", () => {
    mockUseWorktrees.mockReturnValue({
      data: [
        {
          task_id: "impl-2",
          branch: "harness/impl-2",
          workspace_path: "/tmp/worktrees/impl-2",
          path_short: "worktrees/impl-2",
          source_repo: "/tmp/repo",
          repo: "majiayu000/harness",
          status: "implementing",
          phase: "implement",
          description: "Implement feature",
          turn: 2,
          max_turns: 8,
          created_at: "2026-04-22T11:00:00Z",
          duration_secs: 120,
          pr_url: null,
          project: "harness",
        },
        {
          task_id: "fail-1",
          branch: "harness/fail-1",
          workspace_path: "/tmp/worktrees/fail-1",
          path_short: "worktrees/fail-1",
          source_repo: "/tmp/repo",
          repo: "majiayu000/harness",
          status: "failed",
          phase: "review",
          description: "Broken review run",
          turn: 5,
          max_turns: 8,
          created_at: "2026-04-22T11:00:00Z",
          duration_secs: 120,
          pr_url: null,
          project: "harness",
        },
        {
          task_id: "review-3",
          branch: "harness/review-3",
          workspace_path: "/tmp/worktrees/review-3",
          path_short: "worktrees/review-3",
          source_repo: "/tmp/repo",
          repo: "majiayu000/harness",
          status: "reviewing",
          phase: "review",
          description: "Review in progress",
          turn: 4,
          max_turns: 8,
          created_at: "2026-04-22T11:00:00Z",
          duration_secs: 120,
          pr_url: null,
          project: "harness",
        },
      ],
      isLoading: false,
      error: null,
    });

    const { container } = renderPage();
    const cards = Array.from(container.querySelectorAll("article")).map((node) => node.textContent ?? "");
    expect(cards[0]).toContain("Broken review run");
    expect(cards[1]).toContain("Review in progress");
    expect(cards[2]).toContain("Implement feature");
  });

  it("shows a submit CTA when there are no active worktrees", () => {
    mockUseWorktrees.mockReturnValue({
      data: [],
      isLoading: false,
      error: null,
    });

    renderPage();
    expect(screen.getByText("No active worktrees")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Open submit tab" })).toHaveAttribute(
      "href",
      "/dashboard?tab=submit",
    );
  });
});
