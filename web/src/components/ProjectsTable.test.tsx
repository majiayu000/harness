import { describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { render, screen, fireEvent } from "@testing-library/react";
import { ProjectsTable } from "./ProjectsTable";
import type { OverviewProject } from "@/types";

const mockNavigate = vi.fn();
vi.mock("react-router-dom", async (importOriginal) => {
  const actual = await importOriginal<typeof import("react-router-dom")>();
  return { ...actual, useNavigate: () => mockNavigate };
});

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

describe("<ProjectsTable>", () => {
  it("renders project id and merged count", () => {
    render(
      <MemoryRouter>
        <ProjectsTable projects={[p]} />
      </MemoryRouter>,
    );
    expect(screen.getByText("harness")).toBeInTheDocument();
    expect(screen.getByText("28")).toBeInTheDocument();
  });
  it("shows empty state when no projects", () => {
    render(
      <MemoryRouter>
        <ProjectsTable projects={[]} />
      </MemoryRouter>,
    );
    expect(screen.getByText(/no projects registered/i)).toBeInTheDocument();
  });
  it("navigates with project query param on row click", () => {
    mockNavigate.mockClear();
    render(
      <MemoryRouter>
        <ProjectsTable projects={[p]} />
      </MemoryRouter>,
    );
    fireEvent.click(screen.getByText("harness").closest("tr")!);
    expect(mockNavigate).toHaveBeenCalledWith("/?project=harness");
  });
});
