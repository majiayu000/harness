import { describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { render, screen, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ProjectsTable } from "./ProjectsTable";
import type { OverviewProject } from "@/types";

const mockNavigate = vi.fn();
vi.mock("react-router-dom", async (importOriginal) => {
  const actual = await importOriginal<typeof import("react-router-dom")>();
  return { ...actual, useNavigate: () => mockNavigate };
});

vi.mock("./RegisterProjectModal", () => ({
  RegisterProjectModal: ({ open }: { open: boolean }) =>
    open ? <div data-testid="register-modal" /> : null,
}));

const p: OverviewProject = {
  id: "harness",
  root: "/srv/repos/harness",
  running: 3,
  queued: 4,
  done: 28,
  failed: 1,
  merged_24h: 28,
  trend: [1, 2, 3, 4],
  avg_score: 92,
  worktrees: null,
  tokens_24h: null,
  agents: [],
  latest_pr: null,
};

function wrap(ui: React.ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>{ui}</MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("<ProjectsTable>", () => {
  it("renders project id and merged count", () => {
    wrap(<ProjectsTable projects={[p]} />);
    expect(screen.getByText("harness")).toBeInTheDocument();
    expect(screen.getByText("28")).toBeInTheDocument();
  });
  it("shows empty state CTA when no projects", () => {
    wrap(<ProjectsTable projects={[]} />);
    expect(screen.getByText("Add your first project")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /add project/i })).toBeInTheDocument();
  });
  it("empty state button opens registration modal", () => {
    wrap(<ProjectsTable projects={[]} />);
    expect(screen.queryByTestId("register-modal")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /add project/i }));
    expect(screen.getByTestId("register-modal")).toBeInTheDocument();
  });
  it("navigates with project query param on row click", () => {
    mockNavigate.mockClear();
    wrap(<ProjectsTable projects={[p]} />);
    fireEvent.click(screen.getByText("harness").closest("tr")!);
    expect(mockNavigate).toHaveBeenCalledWith("/?project=harness");
  });
});
