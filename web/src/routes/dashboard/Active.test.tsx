import { beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { render, screen, within, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Active } from "./Active";
import type { Task } from "@/types";

vi.mock("@/lib/queries", () => ({
  useTasks: vi.fn(),
  useDashboard: vi.fn(),
}));

vi.mock("@/components/TaskDetailSlideover", () => ({
  TaskDetailSlideover: ({
    taskId,
    onClose,
  }: {
    taskId: string | null;
    onClose: () => void;
  }) => {
    if (!taskId) return null;
    return (
      <div data-testid="task-slideover" data-task-id={taskId}>
        <div data-testid="slideover-scrim" onClick={onClose} />
        <button onClick={onClose}>close-slideover</button>
      </div>
    );
  },
}));

import { useTasks, useDashboard } from "@/lib/queries";

const mockUseTasks = useTasks as ReturnType<typeof vi.fn>;
const mockUseDashboard = useDashboard as ReturnType<typeof vi.fn>;

function makeTask(id: string, project: string | null, status = "running", task_kind = "issue"): Task {
  return {
    id,
    task_kind,
    status,
    turn: 1,
    pr_url: null,
    error: null,
    source: null,
    parent_id: null,
    external_id: null,
    repo: null,
    description: id,
    created_at: null,
    phase: null,
    depends_on: [],
    subtask_ids: [],
    project,
    workflow: null,
  };
}

function columnCount(label: string): string {
  const header = screen.getByText(label).parentElement;
  if (!header) throw new Error(`missing ${label} column header`);
  return within(header).getAllByText(/\d+/)[0].textContent ?? "";
}

function wrap(ui: React.ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>{ui}</MemoryRouter>
    </QueryClientProvider>,
  );
}

const tasks = [
  makeTask("t1", "harness"),
  makeTask("t2", "other-project"),
  makeTask("t3", "harness"),
  makeTask("t4", null),
];

describe("<Active>", () => {
  beforeEach(() => {
    mockUseDashboard.mockReturnValue({ data: undefined });
  });

  it("filters to matching project when projectFilter is set", () => {
    mockUseTasks.mockReturnValue({ data: tasks, isLoading: false, isError: false });
    wrap(<Active projectFilter="harness" />);
    expect(screen.getByText("t1")).toBeInTheDocument();
    expect(screen.getByText("t3")).toBeInTheDocument();
    expect(screen.queryByText("t2")).not.toBeInTheDocument();
    expect(screen.queryByText("t4")).not.toBeInTheDocument();
  });

  it("shows all non-terminal tasks when no projectFilter", () => {
    mockUseTasks.mockReturnValue({ data: tasks, isLoading: false, isError: false });
    wrap(<Active />);
    expect(screen.getByText("t1")).toBeInTheDocument();
    expect(screen.getByText("t2")).toBeInTheDocument();
    expect(screen.getByText("t3")).toBeInTheDocument();
    expect(screen.getByText("t4")).toBeInTheDocument();
  });

  it("shows empty columns when projectFilter matches no tasks", () => {
    mockUseTasks.mockReturnValue({ data: tasks, isLoading: false, isError: false });
    wrap(<Active projectFilter="nonexistent" />);
    expect(screen.queryByText("t1")).not.toBeInTheDocument();
    expect(screen.queryByText("t2")).not.toBeInTheDocument();
    const dashes = screen.getAllByText("—");
    expect(dashes.length).toBeGreaterThan(0);
  });

  it("groups tasks by workflow state before falling back to task status", () => {
    const ready = {
      ...makeTask("ready-task", "harness", "implementing"),
      workflow: { state: "ready_to_merge", pr_number: 123 },
    };
    const feedback = {
      ...makeTask("feedback-task", "harness", "pending"),
      workflow: { state: "addressing_feedback", pr_number: 124 },
    };
    mockUseTasks.mockReturnValue({ data: [ready, feedback], isLoading: false, isError: false });
    wrap(<Active projectFilter="harness" />);

    expect(screen.getByText("wf Ready To Merge")).toBeInTheDocument();
    expect(screen.getByText("wf Addressing Feedback")).toBeInTheDocument();
    expect(columnCount("Ready")).toBe("1");
    expect(columnCount("Feedback")).toBe("1");
    expect(columnCount("Implementing")).toBe("0");
    expect(columnCount("Pending")).toBe("0");
  });

  it("groups planner and review lifecycle statuses outside implementing", () => {
    mockUseTasks.mockReturnValue({
      data: [
        makeTask("planner-task", "harness", "planner_waiting", "planner"),
        makeTask("review-task", "harness", "review_generating", "review"),
        makeTask("impl-task", "harness", "implementing", "issue"),
      ],
      isLoading: false,
      isError: false,
    });
    wrap(<Active />);
    expect(screen.getByText("Planning")).toBeInTheDocument();
    expect(screen.getByText("Review")).toBeInTheDocument();
    expect(screen.getByText("planner-task")).toBeInTheDocument();
    expect(screen.getByText("review-task")).toBeInTheDocument();
    expect(screen.getByText("impl-task")).toBeInTheDocument();
  });

  it("clicking a TaskCard opens the slide-over with that task's id", () => {
    mockUseTasks.mockReturnValue({ data: [makeTask("t1", "proj")], isLoading: false, isError: false });
    wrap(<Active />);
    fireEvent.click(screen.getByText("t1"));
    expect(screen.getByTestId("task-slideover")).toHaveAttribute("data-task-id", "t1");
  });

  it("calling onClose from the slide-over hides it", () => {
    mockUseTasks.mockReturnValue({ data: [makeTask("t1", "proj")], isLoading: false, isError: false });
    wrap(<Active />);
    fireEvent.click(screen.getByText("t1"));
    expect(screen.getByTestId("task-slideover")).toBeInTheDocument();
    fireEvent.click(screen.getByText("close-slideover"));
    expect(screen.queryByTestId("task-slideover")).not.toBeInTheDocument();
  });

  it("clicking the scrim overlay closes the slide-over", () => {
    mockUseTasks.mockReturnValue({ data: [makeTask("t1", "proj")], isLoading: false, isError: false });
    wrap(<Active />);
    fireEvent.click(screen.getByText("t1"));
    expect(screen.getByTestId("task-slideover")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("slideover-scrim"));
    expect(screen.queryByTestId("task-slideover")).not.toBeInTheDocument();
  });
});
