import { fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { Evals } from "./Evals";
import type { EvalDashboardRow, QualitySnapshot } from "@/types";

vi.mock("@/lib/queries", () => ({
  useEvalDashboard: vi.fn(),
}));

import { useEvalDashboard } from "@/lib/queries";

const mockUseEvalDashboard = useEvalDashboard as ReturnType<typeof vi.fn>;

function snapshot(overrides: Partial<QualitySnapshot> = {}): QualitySnapshot {
  return {
    scenario: "pr_repair",
    run_mode: "live_run",
    target: {
      kind: "pull_request",
      repo: "owner/repo",
      pr_number: 7,
      base_ref: "main",
      head_ref: "fix/review",
    },
    baseline_pr: null,
    final_pr: {
      repo: "owner/repo",
      pr_number: 7,
      url: "https://github.com/owner/repo/pull/7",
      title: "Fix review",
      base_ref: "main",
      head_ref: "fix/review",
      head_oid: "abcdef1234567890",
      is_draft: false,
      merge_state: "clean",
      check_state: "passing",
      review_decision: "approved",
      active_unresolved_review_threads: [],
      review_threads_complete: true,
      changed_files: [],
      changed_files_complete: true,
      collected_at: "2026-06-06T00:01:00Z",
    },
    runtime: {
      task_id: "task-7",
      workflow_id: "workflow-7",
      workflow_state: "ready_to_merge",
      latest_activity: "quality_gate",
      terminal_state: "ready_to_merge",
      collected_at: "2026-06-06T00:01:00Z",
    },
    hard_gates: [
      {
        name: "target_correctness",
        status: "pass",
        grade_cap: null,
        message: "final PR matches the requested target",
      },
      {
        name: "review_thread_closure",
        status: "pass",
        grade_cap: null,
        message: "review threads were closed",
      },
    ],
    final_score: 96,
    final_grade: "A",
    grade_cap: null,
    blocker_summary: [],
    usage: [
      {
        agent_invocation_id: "agent-7",
        runtime_job_id: "job-7",
        workflow_id: "workflow-7",
        model: "codex-test",
        reasoning_effort: "medium",
        input_tokens: 1000,
        output_tokens: 500,
        cached_input_tokens: 0,
        total_tokens: 1500,
        cost_usd_micros: 1234,
        token_confidence: "exact",
        cost_confidence: "estimated",
      },
    ],
    ...overrides,
  };
}

function row(id: string, quality: QualitySnapshot | null, error: string | null = null): EvalDashboardRow {
  return {
    run: {
      id,
      scenario: quality?.scenario ?? "pr_repair",
      target:
        quality?.target ?? {
          kind: "pull_request",
          repo: "owner/repo",
          pr_number: 7,
          base_ref: "main",
          head_ref: "fix/review",
        },
      source_task_id: "task-7",
      status: quality ? "scored" : "created",
      quality_snapshot_id: quality ? `snapshot-${id}` : null,
      created_at: "2026-06-06T00:00:00Z",
      updated_at: "2026-06-06T00:02:00Z",
      completed_at: quality ? "2026-06-06T00:02:00Z" : null,
    },
    quality_snapshot: quality
      ? {
          id: `snapshot-${id}`,
          run_id: id,
          snapshot: quality,
          created_at: "2026-06-06T00:02:00Z",
        }
      : null,
    quality_snapshot_error: error,
  };
}

describe("<Evals>", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders scored eval rows with score, gates, usage, and PR target", () => {
    mockUseEvalDashboard.mockReturnValue({
      data: { rows: [row("run-1", snapshot())] },
      isLoading: false,
      isError: false,
    });

    render(<Evals />);

    expect(mockUseEvalDashboard).toHaveBeenCalledWith(50);
    expect(screen.getByText("1 eval runs")).toBeInTheDocument();
    expect(screen.getByText("owner/repo#7")).toHaveAttribute(
      "href",
      "https://github.com/owner/repo/pull/7",
    );
    expect(screen.getByText("A")).toBeInTheDocument();
    expect(screen.getByText("96/100")).toBeInTheDocument();
    expect(screen.getByText("2/2")).toBeInTheDocument();
    expect(screen.getByText("1,500 tokens")).toBeInTheDocument();
    expect(screen.getByText("$0.0012")).toBeInTheDocument();
  });

  it("filters blocked runs from failed hard gates and snapshot errors", () => {
    const blocked = snapshot({
      final_score: 74,
      final_grade: "C",
      blocker_summary: ["active unresolved review threads remain"],
      hard_gates: [
        {
          name: "review_thread_closure",
          status: "fail",
          grade_cap: "C",
          message: "active unresolved review threads remain",
        },
      ],
    });
    mockUseEvalDashboard.mockReturnValue({
      data: {
        rows: [
          row("run-pass", snapshot()),
          row("run-blocked", blocked),
          row("run-error", null, "snapshot unavailable"),
        ],
      },
      isLoading: false,
      isError: false,
    });

    render(<Evals />);
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "blocked" } });

    expect(screen.queryByText("run-pass")).not.toBeInTheDocument();
    expect(screen.getByText("run-blocked")).toBeInTheDocument();
    expect(screen.getByText("snapshot unavailable")).toBeInTheDocument();
    expect(screen.getAllByText("blocked").length).toBeGreaterThanOrEqual(2);
  });

  it("shows empty state when no eval rows match search", () => {
    mockUseEvalDashboard.mockReturnValue({
      data: { rows: [row("run-1", snapshot())] },
      isLoading: false,
      isError: false,
    });

    render(<Evals />);
    fireEvent.change(screen.getByPlaceholderText("Search evals…"), {
      target: { value: "missing" },
    });

    expect(screen.getByText("No eval runs found.")).toBeInTheDocument();
    const metrics = screen.getByText("Runs").parentElement;
    if (!metrics) throw new Error("missing metrics");
    expect(within(metrics).getByText("1")).toBeInTheDocument();
  });
});
