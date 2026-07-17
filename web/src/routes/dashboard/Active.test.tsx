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
    scheduler: {
      authority_state: "running",
      owner: null,
      run_generation: 1,
      recovery_generation: 0,
      lease_expires_at: null,
    },
  };
}

function taskList(data: Task[]) {
  return {
    data,
    page: { limit: 200, has_more: false, next_cursor: null },
    counts: {
      total: data.length,
      running: data.filter((task) => task.scheduler.authority_state === "running").length,
      failed: data.filter((task) => task.status === "failed").length,
      by_status: {},
      by_scheduler_state: {},
    },
  };
}

function columnCount(label: string): string {
  const header = screen.getByText(label).parentElement;
  if (!header) throw new Error(`missing ${label} column header`);
  return within(header).getAllByText(/\d+/)[0].textContent ?? "";
}

function makeQueryClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

function wrap(ui: React.ReactElement, qc = makeQueryClient()) {
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

  it("requests project-scoped active tasks when projectFilter is set", () => {
    mockUseTasks.mockReturnValue({
      data: taskList([tasks[0], tasks[2]]),
      isLoading: false,
      isError: false,
    });
    wrap(<Active projectFilter="harness" />);
    expect(mockUseTasks).toHaveBeenCalledWith({
      active: true,
      limit: 200,
      project_id: "harness",
    });
    expect(screen.getByText("t1")).toBeInTheDocument();
    expect(screen.getByText("t3")).toBeInTheDocument();
    expect(screen.queryByText("t2")).not.toBeInTheDocument();
    expect(screen.queryByText("t4")).not.toBeInTheDocument();
  });

  it("shows all non-terminal tasks when no projectFilter", () => {
    mockUseTasks.mockReturnValue({ data: taskList(tasks), isLoading: false, isError: false });
    wrap(<Active />);
    expect(screen.getByText("t1")).toBeInTheDocument();
    expect(screen.getByText("t2")).toBeInTheDocument();
    expect(screen.getByText("t3")).toBeInTheDocument();
    expect(screen.getByText("t4")).toBeInTheDocument();
  });

  it("shows empty columns when projectFilter matches no tasks", () => {
    mockUseTasks.mockReturnValue({ data: taskList([]), isLoading: false, isError: false });
    wrap(<Active projectFilter="nonexistent" />);
    expect(mockUseTasks).toHaveBeenCalledWith({
      active: true,
      limit: 200,
      project_id: "nonexistent",
    });
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
    mockUseTasks.mockReturnValue({
      data: taskList([ready, feedback]),
      isLoading: false,
      isError: false,
    });
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
    mockUseTasks.mockReturnValue({ data: taskList([ready]), isLoading: false, isError: false });

    wrap(<Active projectFilter="harness" />);

    expect(screen.getByText("wf Ready To Merge")).toBeInTheDocument();
    expect(screen.getByText("tier C")).toBeInTheDocument();
    expect(screen.getByText("fallback: silence")).toBeInTheDocument();
    expect(columnCount("Ready")).toBe("1");

    expect(screen.queryByRole("button", { name: "Merge" })).not.toBeInTheDocument();
    expect(mockApiFetch).not.toHaveBeenCalled();
    expect(screen.queryByTestId("task-slideover")).not.toBeInTheDocument();
  });

  it("uses the workflow runtime merge endpoint for runtime issue workflows", async () => {
    const ready = {
      ...makeTask("runtime-task", "harness", "waiting"),
      pr_url: "https://github.com/owner/repo/pull/124",
      workflow: {
        id: "runtime-workflow-124",
        definition_id: "github_issue_pr",
        state: "ready_to_merge",
        pr_number: 124,
      },
    };
    mockUseTasks.mockReturnValue({ data: taskList([ready]), isLoading: false, isError: false });

    wrap(<Active projectFilter="harness" />);
    fireEvent.click(screen.getByRole("button", { name: "Merge" }));

    await waitFor(() => {
      expect(mockApiFetch).toHaveBeenCalledWith("/api/workflows/runtime/merge", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ workflow_id: "runtime-workflow-124" }),
      });
    });
  });

  it("opens runtime rows by submission_id when present", () => {
    const runtimeTask = {
      ...makeTask("legacy-task-alias", "harness", "implementing"),
      submission_id: "runtime-submission-handle",
      execution_path: "workflow_runtime",
      workflow_id: "runtime-workflow-1128",
      workflow: {
        id: "runtime-workflow-1128",
        definition_id: "github_issue_pr",
        state: "implementing",
      },
    };
    mockUseTasks.mockReturnValue({
      data: taskList([runtimeTask]),
      isLoading: false,
      isError: false,
    });

    wrap(<Active projectFilter="harness" />);
    fireEvent.click(screen.getByText("legacy-task-alias"));

    expect(screen.getByTestId("task-slideover")).toHaveAttribute(
      "data-task-id",
      "runtime-submission-handle",
    );
  });

  it("groups planner and review lifecycle statuses outside implementing", () => {
    mockUseTasks.mockReturnValue({
      data: taskList([
        makeTask("planner-task", "harness", "planner_waiting", "planner"),
        makeTask("review-task", "harness", "review_generating", "review"),
        makeTask("impl-task", "harness", "triaging", "issue"),
      ]),
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
    mockUseTasks.mockReturnValue({ data: taskList([]), isLoading: false, isError: false });
    mockUseWorkflowRuntimeTree.mockReturnValue({
      data: {
        total_workflows: 2,
        pagination: {
          limit: 100,
          offset: 0,
          returned: 2,
          total: 2,
          has_more: false,
          next_offset: null,
          job_limit: 5,
        },
        summary: {
          total_commands: 1,
          total_runtime_jobs: 7,
          command_statuses: { pending: 1 },
          runtime_job_statuses: { failed: 3, running: 4 },
          running_job_lease_statuses: { active_leased: 2, expired_lease: 2 },
        },
        workflows: [
          {
            workflow: {
              id: "prompt-task",
              definition_id: "prompt_task",
              definition_version: 1,
              state: "implementing",
              subject: { subject_type: "prompt", subject_key: "owner/repo" },
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
                  parent_workflow_id: "prompt-task",
                  data: {},
                  version: 2,
                  created_at: "2026-04-30T00:00:00Z",
                  updated_at: "2026-04-30T00:00:00Z",
                },
                rejected_decision_count: 6,
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
                    runtime_job_count: 7,
                    runtime_jobs: [
                      {
                        id: "job-1",
                        command_id: "command-1",
                        runtime_kind: "codex_jsonrpc",
                        runtime_profile: "codex-high",
                        status: "failed",
                        input: {},
                        not_before: "2026-04-30T00:05:00Z",
                        runtime_event_count: 2,
                        latest_runtime_event_type: "ActivityFailed",
                        prompt_packet_digest:
                          "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                        created_at: "2026-04-30T00:00:00Z",
                        updated_at: "2026-04-30T00:01:00Z",
                      },
                      {
                        id: "job-2",
                        command_id: "command-1",
                        runtime_kind: "codex_jsonrpc",
                        runtime_profile: "codex-high",
                        status: "running",
                        lease_state: "active_leased",
                        in_flight_model_turn: true,
                        last_runtime_observation_at: "2026-04-30T00:03:30Z",
                        input: {},
                        not_before: null,
                        runtime_event_count: 3,
                        latest_runtime_event_type: "ActivityStarted",
                        prompt_packet_digest:
                          "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                        created_at: "2026-04-30T00:02:00Z",
                        updated_at: "2026-04-30T00:03:00Z",
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
    expect(screen.getByText("2/2 workflows")).toBeInTheDocument();
    expect(screen.getByText("2 active leases")).toBeInTheDocument();
    expect(screen.getByText("2 expired/missing")).toBeInTheDocument();
    expect(screen.getByText("prompt_task - owner/repo")).toBeInTheDocument();
    expect(screen.getByText("github_issue_pr - issue:123")).toBeInTheDocument();
    expect(screen.getByText("rejected 6")).toBeInTheDocument();
    expect(screen.getByText("activity: replan_issue")).toBeInTheDocument();
    expect(screen.getByText("7 jobs")).toBeInTheDocument();
    expect(
      screen.getByText(
        "2/7 jobs - running - active leased - in-flight - observed 2026-04-30 00:03:30Z - event ActivityStarted - prompt abcdef012345",
      ),
    ).toBeInTheDocument();
    expect(screen.getByText("rejected: replan limit exhausted")).toBeInTheDocument();
  });

  it("cancels a non-terminal runtime issue workflow from the runtime panel", async () => {
    mockUseTasks.mockReturnValue({ data: taskList([]), isLoading: false, isError: false });
    mockUseWorkflowRuntimeTree.mockReturnValue({
      data: {
        total_workflows: 1,
        workflows: [
          {
            workflow: {
              id: "runtime-issue-901",
              definition_id: "github_issue_pr",
              definition_version: 1,
              state: "implementing",
              subject: { subject_type: "issue", subject_key: "issue:901" },
              data: { task_id: "runtime-task-901" },
              version: 1,
              created_at: "2026-04-30T00:00:00Z",
              updated_at: "2026-04-30T00:00:00Z",
            },
            events: [],
            decisions: [],
            commands: [],
            children: [],
          },
        ],
      },
      isLoading: false,
      isError: false,
    });

    wrap(<Active projectFilter="harness" />);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    await waitFor(() => {
      expect(mockApiFetch).toHaveBeenCalledWith("/api/workflows/runtime/cancel", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ workflow_id: "runtime-issue-901" }),
      });
    });
  });

  it("refreshes runtime data when workflow cancellation returns a conflict", async () => {
    mockApiFetch.mockRejectedValueOnce(new Error("workflow already terminal"));
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
    const qc = makeQueryClient();
    const invalidateQueries = vi.spyOn(qc, "invalidateQueries");
    mockUseTasks.mockReturnValue({ data: taskList([]), isLoading: false, isError: false });
    mockUseWorkflowRuntimeTree.mockReturnValue({
      data: {
        total_workflows: 1,
        workflows: [
          {
            workflow: {
              id: "runtime-issue-conflict",
              definition_id: "github_issue_pr",
              definition_version: 1,
              state: "implementing",
              subject: { subject_type: "issue", subject_key: "issue:902" },
              data: { task_id: "runtime-task-902" },
              version: 1,
              created_at: "2026-04-30T00:00:00Z",
              updated_at: "2026-04-30T00:00:00Z",
            },
            events: [],
            decisions: [],
            commands: [],
            children: [],
          },
        ],
      },
      isLoading: false,
      isError: false,
    });

    wrap(<Active projectFilter="harness" />, qc);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    await waitFor(() => {
      expect(mockApiFetch).toHaveBeenCalledWith("/api/workflows/runtime/cancel", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ workflow_id: "runtime-issue-conflict" }),
      });
    });
    await waitFor(() => {
      expect(invalidateQueries).toHaveBeenCalledWith({ queryKey: ["tasks"] });
      expect(invalidateQueries).toHaveBeenCalledWith({ queryKey: ["workflow-runtime-tree"] });
    });
    expect(consoleError).toHaveBeenCalledWith(
      "Failed to cancel runtime workflow",
      expect.any(Error),
    );
    consoleError.mockRestore();
  });

  it("clicking a standard task card opens the slide-over with that task's id", () => {
    mockUseTasks.mockReturnValue({
      data: taskList([makeTask("t1", "proj")]),
      isLoading: false,
      isError: false,
    });
    wrap(<Active />);
    fireEvent.click(screen.getByText("t1"));
    expect(screen.getByTestId("task-slideover")).toHaveAttribute("data-task-id", "t1");
  });

  it("clicking a prompt task card opens the shared slide-over", () => {
    const promptTask = makeTask("prompt-task", "proj", "planning", "prompt");
    mockUseTasks.mockReturnValue({ data: taskList([promptTask]), isLoading: false, isError: false });
    wrap(<Active />);
    fireEvent.click(screen.getByText("prompt-task"));
    expect(screen.getByTestId("task-slideover")).toHaveAttribute("data-task-id", "prompt-task");
  });

  it("calling onClose from the slide-over hides it", () => {
    mockUseTasks.mockReturnValue({
      data: taskList([makeTask("t1", "proj")]),
      isLoading: false,
      isError: false,
    });
    wrap(<Active />);
    fireEvent.click(screen.getByText("t1"));
    expect(screen.getByTestId("task-slideover")).toBeInTheDocument();
    fireEvent.click(screen.getByText("close-slideover"));
    expect(screen.queryByTestId("task-slideover")).not.toBeInTheDocument();
  });

  it("clicking the scrim overlay closes the slide-over", () => {
    mockUseTasks.mockReturnValue({
      data: taskList([makeTask("t1", "proj")]),
      isLoading: false,
      isError: false,
    });
    wrap(<Active />);
    fireEvent.click(screen.getByText("t1"));
    expect(screen.getByTestId("task-slideover")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("slideover-scrim"));
    expect(screen.queryByTestId("task-slideover")).not.toBeInTheDocument();
  });
});
