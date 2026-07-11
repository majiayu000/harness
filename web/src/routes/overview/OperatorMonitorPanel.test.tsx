import { beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { OperatorMonitorPanel } from "./OperatorMonitorPanel";
import type { OperatorMonitorPayload } from "@/types";

vi.mock("@/lib/queries", () => ({
  useOperatorMonitor: vi.fn(),
}));

vi.mock("@/lib/api", () => ({
  apiFetch: vi.fn(),
}));

import { useOperatorMonitor } from "@/lib/queries";
import { apiFetch } from "@/lib/api";

const mockUseOperatorMonitor = useOperatorMonitor as ReturnType<typeof vi.fn>;
const mockApiFetch = apiFetch as ReturnType<typeof vi.fn>;

function wrap(
  ui: React.ReactElement,
  qc = new QueryClient({ defaultOptions: { queries: { retry: false } } }),
) {
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

function makeMonitor(): OperatorMonitorPayload {
  return {
    generated_at: "2026-06-12T00:00:00Z",
    sample_limit: 500,
    health: {
      status: "ok",
      degraded_subsystems: [],
      runtime_log_state: "enabled",
      runtime_log_path: "/tmp/harness.log",
      uptime_secs: 120,
      runtime_hosts_online: 1,
      runtime_hosts_total: 1,
    },
    activity: {
      runtime_workflows: {
        pending: 0,
        running: 2,
        review: 1,
        awaiting_dependencies: 0,
        ready_to_merge: 1,
        blocked: 0,
        failed: 0,
        done: 4,
        other: 0,
      },
      legacy_queue: {
        queued: 0,
        running: 0,
        stalled: 0,
        failed: 0,
        done: 0,
      },
      by_source: [
        {
          source: "github",
          pending: 1,
          running: 2,
          review: 1,
          ready_to_merge: 1,
          blocked: 0,
          failed: 0,
        },
      ],
      token_dispatch_by_repo: [],
    },
    operator_actions: [
      {
        kind: "ready_to_merge",
        repo: "owner/repo",
        issue: 42,
        pr: 43,
        task_id: "task-42",
        workflow_id: "workflow-42",
        state: "ready_to_merge",
        age_secs: 900,
        url: "https://github.com/owner/repo/pull/43",
        evidence_url: "/tasks/task-42",
        next_action: "Review and merge",
        source: "github",
      },
    ],
    stuck_workflows: [
      {
        workflow_id: "workflow-stuck",
        definition_id: "github_issue_pr",
        state: "awaiting_feedback",
        repo: "owner/repo",
        issue: 44,
        pr: 45,
        age_secs: 18_000,
        updated_at: "2026-06-11T19:00:00Z",
        url: "https://github.com/owner/repo/pull/45",
        source: "github",
      },
    ],
    failures: [
      {
        family: "github_fetch",
        severity: "warn",
        message: "git fetch origin main failed: SSL_ERROR_SYSCALL in connection to github.com:443",
        first_seen: "2026-06-12T00:00:00Z",
        last_seen: "2026-06-12T00:05:00Z",
        count: 2,
        repo: "repo",
        task_id: "task-err",
        retryable: true,
        next_action: "Retry after GitHub connectivity recovers",
      },
    ],
    worktrees: {
      used: 1,
      capacity: 4,
      stale: 0,
      metrics_state: "unavailable",
      cards: [
        {
          task_id: "task-42",
          branch: "harness/task-42",
          workspace_path: "/tmp/workspaces/task-42",
          path_short: "workspaces/task-42",
          source_repo: "/tmp/repo",
          repo: "owner/repo",
          runtime_workflow_id: "workflow-42",
          status: "implementing",
          phase: "implement",
          description: "Implement operator monitor",
          turn: 1,
          max_turns: 8,
          created_at: "2026-06-12T00:00:00Z",
          duration_secs: 120,
          pr_url: "https://github.com/owner/repo/pull/43",
          project: "/tmp/repo",
        },
      ],
    },
  };
}

describe("<OperatorMonitorPanel>", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockApiFetch.mockResolvedValue(new Response("{}", { status: 200 }));
  });

  it("renders active runtime work and ready-to-merge operator actions", () => {
    mockUseOperatorMonitor.mockReturnValue({
      data: makeMonitor(),
      isError: false,
    });

    wrap(<OperatorMonitorPanel />);

    expect(screen.getByText("runtime running")).toBeInTheDocument();
    expect(screen.getByText("ready to merge")).toBeInTheDocument();
    expect(screen.getByText("#42 -> PR #43")).toBeInTheDocument();
    expect(screen.getAllByRole("link", { name: "open" })[0]).toHaveAttribute(
      "href",
      "https://github.com/owner/repo/pull/43",
    );
    expect(screen.getByText("awaiting feedback")).toBeInTheDocument();
    expect(screen.getByText("#44 -> PR #45")).toBeInTheDocument();
  });

  it("renders grouped retryable GitHub fetch failures", () => {
    mockUseOperatorMonitor.mockReturnValue({
      data: makeMonitor(),
      isError: false,
    });

    wrap(<OperatorMonitorPanel />);

    expect(screen.getByText("github_fetch")).toBeInTheDocument();
    expect(screen.getByText(/SSL_ERROR_SYSCALL/)).toBeInTheDocument();
    expect(screen.getByText("retryable")).toBeInTheDocument();
  });

  it("shows metrics-unavailable worktree state explicitly", () => {
    mockUseOperatorMonitor.mockReturnValue({
      data: makeMonitor(),
      isError: false,
    });

    wrap(<OperatorMonitorPanel />);

    expect(screen.getByText(/metrics unavailable/)).toBeInTheDocument();
    expect(screen.getByText("workspaces/task-42")).toBeInTheDocument();
  });

  it("shows unavailable copy when the monitor query fails", () => {
    mockUseOperatorMonitor.mockReturnValue({
      data: undefined,
      isError: true,
    });

    wrap(<OperatorMonitorPanel />);

    expect(screen.getByText("Operator monitor unavailable.")).toBeInTheDocument();
    expect(screen.queryByText("No current operator actions.")).not.toBeInTheDocument();
  });

  it("renders current-state stop details and invokes only eligible recovery actions", async () => {
    const monitor = makeMonitor();
    const baseAction = monitor.operator_actions[0];
    monitor.operator_actions = [
      {
        ...baseAction,
        kind: "blocked",
        workflow_id: "workflow-blocked",
        state: "blocked",
        blocked_reason: "Waiting for maintainer approval.",
        unblock_hint: "Post the approval comment, then unblock.",
        failure_reason: "Stale failed reason.",
        retry_hint: "Stale retry hint.",
        last_stop: { state: "failed" },
        can_unblock: true,
        can_retry: false,
      },
      {
        ...baseAction,
        kind: "failed",
        workflow_id: "workflow-retryable",
        state: "failed",
        blocked_reason: "Stale blocked reason.",
        unblock_hint: "Stale unblock hint.",
        failure_reason: "Runtime transport timed out.",
        retry_hint: "Retry after transport recovers.",
        last_stop: { state: "blocked" },
        can_unblock: false,
        can_retry: true,
      },
      {
        ...baseAction,
        kind: "failed",
        workflow_id: "workflow-nonretryable",
        state: "failed",
        failure_reason: "Runtime configuration is invalid.",
        can_unblock: false,
        can_retry: false,
      },
    ];
    monitor.stuck_workflows[0] = {
      ...monitor.stuck_workflows[0],
      blocked_reason: "Aged workflow still needs approval.",
      failure_reason: "Stale aged failure.",
      last_stop: { state: "blocked" },
    };
    mockUseOperatorMonitor.mockReturnValue({ data: monitor, isError: false });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const invalidateQueries = vi.spyOn(queryClient, "invalidateQueries");

    wrap(<OperatorMonitorPanel />, queryClient);

    expect(screen.getByText("Waiting for maintainer approval.")).toBeInTheDocument();
    expect(screen.getByText("Post the approval comment, then unblock.")).toBeInTheDocument();
    expect(screen.getByText("Runtime transport timed out.")).toBeInTheDocument();
    expect(screen.getByText("Retry after transport recovers.")).toBeInTheDocument();
    expect(screen.getByText("Aged workflow still needs approval.")).toBeInTheDocument();
    for (const staleText of [
      "Stale failed reason.",
      "Stale retry hint.",
      "Stale blocked reason.",
      "Stale unblock hint.",
      "Stale aged failure.",
    ]) {
      expect(screen.queryByText(staleText)).not.toBeInTheDocument();
    }
    expect(
      screen.queryByRole("button", { name: "Retry workflow workflow-nonretryable" }),
    ).not.toBeInTheDocument();

    const unblockButton = screen.getByRole("button", {
      name: "Unblock workflow workflow-blocked",
    });
    fireEvent.click(unblockButton);
    expect(screen.getByText("Recovery reason is required.")).toBeInTheDocument();
    expect(mockApiFetch).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText("Recovery reason for workflow-blocked"), {
      target: { value: "  Approval was posted  " },
    });
    fireEvent.click(unblockButton);

    await waitFor(() =>
      expect(mockApiFetch).toHaveBeenNthCalledWith(1, "/api/workflows/runtime/unblock", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          workflow_id: "workflow-blocked",
          reason: "Approval was posted",
        }),
      }),
    );
    await waitFor(() => expect(unblockButton).toBeEnabled());

    fireEvent.change(screen.getByLabelText("Recovery reason for workflow-retryable"), {
      target: { value: "Transport is healthy" },
    });
    fireEvent.click(
      screen.getByRole("button", { name: "Retry workflow workflow-retryable" }),
    );

    await waitFor(() =>
      expect(mockApiFetch).toHaveBeenNthCalledWith(2, "/api/workflows/runtime/retry", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          workflow_id: "workflow-retryable",
          reason: "Transport is healthy",
        }),
      }),
    );
    expect(invalidateQueries).toHaveBeenCalledWith({ queryKey: ["operator-monitor"] });
    expect(invalidateQueries).toHaveBeenCalledWith({ queryKey: ["workflow-runtime-tree"] });
  });

  it("shows recovery failures instead of swallowing them", async () => {
    const monitor = makeMonitor();
    monitor.operator_actions[0] = {
      ...monitor.operator_actions[0],
      kind: "blocked",
      workflow_id: "workflow-blocked",
      state: "blocked",
      can_unblock: true,
    };
    mockUseOperatorMonitor.mockReturnValue({ data: monitor, isError: false });
    mockApiFetch.mockRejectedValueOnce(new Error("workflow not in blocked state"));

    wrap(<OperatorMonitorPanel />);
    fireEvent.change(screen.getByLabelText("Recovery reason for workflow-blocked"), {
      target: { value: "Approval was posted" },
    });
    fireEvent.click(
      screen.getByRole("button", { name: "Unblock workflow workflow-blocked" }),
    );

    expect(
      await screen.findByText(
        "Workflow recovery failed: workflow not in blocked state",
      ),
    ).toBeInTheDocument();
  });
});
