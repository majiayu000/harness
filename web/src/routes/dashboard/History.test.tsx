import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { History } from "./History";
import type { Task } from "@/types";

vi.mock("@/lib/queries", () => ({
  useDashboard: vi.fn(),
  useTasks: vi.fn(),
}));

import { useDashboard, useTasks } from "@/lib/queries";

const mockUseDashboard = useDashboard as ReturnType<typeof vi.fn>;
const mockUseTasks = useTasks as ReturnType<typeof vi.fn>;

function makeHistoryTask(id: string, overrides: Partial<Task> = {}): Task {
  return {
    id,
    task_kind: "issue",
    status: "done",
    turn: 1,
    pr_url: null,
    error: null,
    source: null,
    parent_id: null,
    external_id: null,
    repo: "owner/repo",
    description: id,
    created_at: "2026-01-01T00:00:00Z",
    phase: null,
    depends_on: [],
    subtask_ids: [],
    project: "harness",
    workflow: null,
    scheduler: {
      authority_state: "completed",
      owner: null,
      run_generation: 1,
      recovery_generation: 0,
      lease_expires_at: null,
    },
    ...overrides,
  };
}

function historyTaskList(data: Task[]) {
  return {
    data,
    page: { limit: 500, has_more: false, next_cursor: null },
    counts: {
      total: data.length,
      running: 0,
      failed: data.filter((task) => task.status === "failed").length,
      by_status: {},
      by_scheduler_state: {},
    },
  };
}

describe("<History>", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockUseDashboard.mockReturnValue({ data: { projects: [] } });
  });

  it("paginates terminal tasks and resets pagination when filtering", () => {
    const doneTasks = Array.from({ length: 21 }, (_, index) =>
      makeHistoryTask(`done-${index}`, {
        description: `Done task ${index}`,
        created_at: new Date(Date.UTC(2026, 0, index + 1)).toISOString(),
      }),
    );
    const failedTask = makeHistoryTask("failed-important", {
      status: "failed",
      created_at: "2026-02-01T00:00:00Z",
      description: "Broken review run",
    });
    mockUseTasks.mockReturnValue({
      data: historyTaskList([...doneTasks, failedTask]),
      isLoading: false,
      isError: false,
    });

    render(<History />);

    expect(mockUseTasks).toHaveBeenCalledWith({
      status: "done,failed,cancelled",
      limit: 500,
      project_id: undefined,
    });
    expect(screen.getByText("page 1/2")).toBeInTheDocument();
    expect(screen.getByText("Broken review run")).toBeInTheDocument();
    expect(screen.getByText("Done task 20")).toBeInTheDocument();
    expect(screen.queryByText("Done task 0")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("page 2/2")).toBeInTheDocument();
    expect(screen.getByText("Done task 0")).toBeInTheDocument();

    fireEvent.change(screen.getByRole("combobox"), { target: { value: "failed" } });
    expect(screen.getByText("page 1/1")).toBeInTheDocument();
    expect(screen.getByText("Broken review run")).toBeInTheDocument();
    expect(screen.queryByText("Done task 0")).not.toBeInTheDocument();
  });

  it("searches description repo and PR URL, then shows the submit CTA when empty", () => {
    mockUseDashboard.mockReturnValue({
      data: { projects: [{ id: "harness", root: "/work/harness" }] },
    });
    mockUseTasks.mockReturnValue({
      data: historyTaskList([
        makeHistoryTask("task-a", {
          description: "Fix auth callback",
          repo: "owner/frontend",
          pr_url: "https://github.com/owner/frontend/pull/42",
        }),
        makeHistoryTask("task-b", {
          description: "Repair worker retry",
          repo: "owner/backend",
          pr_url: "https://github.com/owner/backend/pull/77",
        }),
      ]),
      isLoading: false,
      isError: false,
    });

    render(<History projectFilter="harness" />);

    expect(mockUseTasks).toHaveBeenCalledWith({
      status: "done,failed,cancelled",
      limit: 500,
      project_id: "/work/harness",
    });

    fireEvent.change(screen.getByPlaceholderText("Search description, repo, PR…"), {
      target: { value: "frontend/pull/42" },
    });
    expect(screen.getByText("Fix auth callback")).toBeInTheDocument();
    expect(screen.queryByText("Repair worker retry")).not.toBeInTheDocument();

    fireEvent.change(screen.getByPlaceholderText("Search description, repo, PR…"), {
      target: { value: "missing" },
    });
    expect(screen.getByText("No terminal tasks found.")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Submit task" })).toHaveAttribute(
      "href",
      "/dashboard?tab=submit&project=harness",
    );
  });
});
