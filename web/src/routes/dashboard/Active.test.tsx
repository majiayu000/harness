import { beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { render, screen, within, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Active } from "./Active";
import type { Task } from "@/types";

vi.mock("@/lib/queries", () => ({
  useTasks: vi.fn(),
  useDashboard: vi.fn(),
  useWorkflowRuntimeTree: vi.fn(),
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

vi.mock("@/lib/api", () => ({
  apiFetch: vi.fn(() => Promise.resolve(undefined)),
}));

import { useTasks, useDashboard, useWorkflowRuntimeTree } from "@/lib/queries";
import { apiFetch } from "@/lib/api";

const mockUseTasks = useTasks as ReturnType<typeof vi.fn>;
const mockUseDashboard = useDashboard as ReturnType<typeof vi.fn>;
const mockUseWorkflowRuntimeTree = useWorkflowRuntimeTree as ReturnType<typeof vi.fn>;
const mockApiFetch = apiFetch as ReturnType<typeof vi.fn>;

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
    vi.clearAllMocks();
    mockUseDashboard.mockReturnValue({ data: undefined });
    mockUseWorkflowRuntimeTree.mockReturnValue({
      data: { workflows: [], total_workflows: 0 },
      isLoading: false,
      isError: false,
    });
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

  it("keeps ready-to-merge terminal tasks visible and mergeable", async () => {
    const ready = {
      ...makeTask("ready-task", "harness", "done"),
      pr_url: "https://github.com/owner/repo/pull/123",
      workflow: {
        state: "ready_to_merge",
        pr_number: 123,
        review_fallback: { tier: "c", trigger: "silence", active_bot: "codex", activated_at: "2026-04-30T00:00:00Z" },
      },
    };
    mockUseTasks.mockReturnValue({ data: [ready], isLoading: false, isError: false });

    wrap(<Active projectFilter="harness" />);

    expect(screen.getByText("wf Ready To Merge")).toBeInTheDocument();
    expect(screen.getByText("tier C")).toBeInTheDocument();
    expect(screen.getByText("fallback: silence")).toBeInTheDocument();
    expect(columnCount("Ready")).toBe("1");

    fireEvent.click(screen.getByRole("button", { name: "Merge" }));

    await waitFor(() => {
      expect(mockApiFetch).toHaveBeenCalledWith("/tasks/ready-task/merge", { method: "POST" });
    });
    expect(screen.queryByTestId("task-slideover")).not.toBeInTheDocument();
  });

  it("groups planner and review lifecycle statuses outside implementing", () => {
    mockUseTasks.mockReturnValue({
      data: [
        makeTask("planner-task", "harness", "planner_waiting", "planner"),
        makeTask("review-task", "harness", "review_generating", "review"),
        makeTask("impl-task", "harness", "triaging", "issue"),
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

  it("renders workflow runtime tree with jobs and rejected decisions", () => {
    mockUseTasks.mockReturnValue({ data: [], isLoading: false, isError: false });
    mockUseWorkflowRuntimeTree.mockReturnValue({
      data: {
        total_workflows: 2,
        workflows: [
          {
            workflow: {
              id: "repo-backlog",
              definition_id: "repo_backlog",
              definition_version: 1,
              state: "dispatching",
              subject: { subject_type: "repo", subject_key: "owner/repo" },
              data: {},
              version: 1,
              created_at: "2026-04-30T00:00:00Z",
              updated_at: "2026-04-30T00:00:00Z",
            },
            events: [],
            decisions: [],
            commands: [],
            children: [
              {
                workflow: {
                  id: "issue-123",
                  definition_id: "github_issue_pr",
                  definition_version: 1,
                  state: "replanning",
                  subject: { subject_type: "issue", subject_key: "issue:123" },
                  parent_workflow_id: "repo-backlog",
                  data: {},
                  version: 2,
                  created_at: "2026-04-30T00:00:00Z",
                  updated_at: "2026-04-30T00:00:00Z",
                },
                events: [
                  {
                    id: "event-1",
                    workflow_id: "issue-123",
                    sequence: 1,
                    event_type: "PlanIssueRaised",
                    event: {},
                    source: "test",
                    created_at: "2026-04-30T00:00:00Z",
                  },
                ],
                decisions: [
                  {
                    id: "decision-1",
                    workflow_id: "issue-123",
                    accepted: false,
                    rejection_reason: "replan limit exhausted",
                    decision: {
                      decision: "run_replan",
                      observed_state: "replanning",
                      next_state: "replanning",
                      reason: "Replan requested after the budget was exhausted.",
                    },
                    created_at: "2026-04-30T00:00:00Z",
                  },
                ],
                commands: [
                  {
                    id: "command-1",
                    workflow_id: "issue-123",
                    decision_id: "decision-1",
                    status: "pending",
                    command: {
                      command_type: "enqueue_activity",
                      dedupe_key: "issue-123-replan-2",
                      command: { activity: "replan_issue" },
                    },
                    runtime_jobs: [
                      {
                        id: "job-1",
                        command_id: "command-1",
                        runtime_kind: "codex_jsonrpc",
                        runtime_profile: "codex-high",
                        status: "pending",
                        input: {},
                        created_at: "2026-04-30T00:00:00Z",
                        updated_at: "2026-04-30T00:00:00Z",
                      },
                    ],
                    created_at: "2026-04-30T00:00:00Z",
                    updated_at: "2026-04-30T00:00:00Z",
                  },
                ],
                children: [],
              },
            ],
          },
        ],
      },
      isLoading: false,
      isError: false,
    });

    wrap(<Active projectFilter="harness" />);

    expect(screen.getByText("Workflow Runtime")).toBeInTheDocument();
    expect(screen.getByText("repo_backlog - owner/repo")).toBeInTheDocument();
    expect(screen.getByText("github_issue_pr - issue:123")).toBeInTheDocument();
    expect(screen.getByText("activity: replan_issue")).toBeInTheDocument();
    expect(screen.getByText("1 jobs - pending")).toBeInTheDocument();
    expect(screen.getByText("rejected: replan limit exhausted")).toBeInTheDocument();
  });

  it("clicking a standard task card opens the slide-over with that task's id", () => {
    mockUseTasks.mockReturnValue({ data: [makeTask("t1", "proj")], isLoading: false, isError: false });
    wrap(<Active />);
    fireEvent.click(screen.getByText("t1"));
    expect(screen.getByTestId("task-slideover")).toHaveAttribute("data-task-id", "t1");
  });

  it("clicking a prompt task card opens the shared slide-over", () => {
    const promptTask = makeTask("prompt-task", "proj", "planning", "prompt");
    mockUseTasks.mockReturnValue({ data: [promptTask], isLoading: false, isError: false });
    wrap(<Active />);
    fireEvent.click(screen.getByText("prompt-task"));
    expect(screen.getByTestId("task-slideover")).toHaveAttribute("data-task-id", "prompt-task");
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
