import { describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import { OperatorMonitorPanel } from "./OperatorMonitorPanel";
import type { OperatorMonitorPayload } from "@/types";

vi.mock("@/lib/queries", () => ({
  useOperatorMonitor: vi.fn(),
}));

import { useOperatorMonitor } from "@/lib/queries";

const mockUseOperatorMonitor = useOperatorMonitor as ReturnType<typeof vi.fn>;

function wrap(ui: React.ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
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
});
