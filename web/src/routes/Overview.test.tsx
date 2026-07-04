import { beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import { Overview } from "./Overview";
import { PaletteProvider } from "@/lib/palette";
import { DOCS_URL } from "@/lib/links";
import type { OperatorMonitorPayload, OperatorSnapshotPayload, OverviewPayload } from "@/types";

vi.mock("@/lib/queries", () => ({
  useOverview: vi.fn(),
  useOperatorMonitor: vi.fn(),
  useOperatorSnapshot: vi.fn(),
}));

import { useOperatorMonitor, useOperatorSnapshot, useOverview } from "@/lib/queries";

const mockUseOverview = useOverview as ReturnType<typeof vi.fn>;
const mockUseOperatorMonitor = useOperatorMonitor as ReturnType<typeof vi.fn>;
const mockUseOperatorSnapshot = useOperatorSnapshot as ReturnType<typeof vi.fn>;

function wrap(ui: React.ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <PaletteProvider>
        <MemoryRouter>{ui}</MemoryRouter>
      </PaletteProvider>
    </QueryClientProvider>,
  );
}

function makeOverview(): OverviewPayload {
  return {
    window: {
      hours: 24,
      since: "2026-04-21T00:00:00Z",
      now: "2026-04-22T00:00:00Z",
    },
    kpi: {
      active_tasks: 1,
      merged_24h: 0,
      avg_review_score: null,
      grade: null,
      rule_fail_rate_pct: 0,
      tokens_24h: null,
      worktrees: {
        used: 0,
        total: 1,
      },
    },
    agent_tokens: [],
    distribution: {
      queued: 0,
      running: 1,
      review: 0,
      merged: 0,
      failed: 0,
      stalled: 0,
    },
    throughput: {
      hours: [],
      series: [],
    },
    projects: [],
    runtimes: [],
    heatmap: {
      bucket_minutes: 60,
      rows: [],
    },
    feed: [],
    alerts: [],
    evolution: null,
    global: {
      uptime_secs: 1,
      running: 1,
      queued: 0,
      done: 0,
      failed: 0,
      stalled: 0,
      max_concurrent: 1,
    },
  };
}

function makeSnapshot(): OperatorSnapshotPayload {
  return {
    generated_at: "2026-04-22T00:00:00Z",
    retry: {
      last_tick: null,
      stalled_tasks: [],
    },
    rate_limits: {
      signal_ingestion: {
        tracked_sources: 0,
        limit_per_minute: 100,
      },
      password_reset: {
        tracked_identifiers: 0,
        limit_per_hour: 5,
      },
    },
    recent_failures: [],
    runtime_logs: {
      state: "disabled",
      active_path: null,
      path_hint: null,
      retention_days: 30,
    },
  };
}

function makeMonitor(): OperatorMonitorPayload {
  return {
    generated_at: "2026-04-22T00:00:00Z",
    sample_limit: 500,
    health: {
      status: "ok",
      degraded_subsystems: [],
      runtime_log_state: "disabled",
      runtime_log_path: null,
      uptime_secs: 1,
      runtime_hosts_online: 0,
      runtime_hosts_total: 0,
    },
    activity: {
      runtime_workflows: {
        pending: 0,
        running: 1,
        review: 0,
        awaiting_dependencies: 0,
        ready_to_merge: 0,
        blocked: 0,
        failed: 0,
        done: 0,
        other: 0,
      },
      legacy_queue: {
        queued: 0,
        running: 1,
        stalled: 0,
        failed: 0,
        done: 0,
      },
      by_source: [],
      token_dispatch_by_repo: [],
    },
    operator_actions: [],
    stuck_workflows: [],
    failures: [],
    worktrees: {
      used: 0,
      capacity: 1,
      stale: 0,
      metrics_state: "unavailable",
      cards: [],
    },
  };
}

describe("<Overview>", () => {
  beforeEach(() => {
    mockUseOperatorMonitor.mockReturnValue({
      data: makeMonitor(),
      isError: false,
    });
  });

  it("shows a degraded status badge when operator snapshot fails even if overview succeeds", () => {
    mockUseOverview.mockReturnValue({
      data: makeOverview(),
      isError: false,
    });
    mockUseOperatorSnapshot.mockReturnValue({
      data: undefined,
      isError: true,
    });

    wrap(<Overview />);

    expect(screen.getByText(/connection lost/i)).toBeInTheDocument();
    expect(screen.queryByText(/all systems nominal/i)).not.toBeInTheDocument();
  });

  it("shows a healthy status badge only when both overview and operator snapshot succeed", () => {
    mockUseOverview.mockReturnValue({
      data: makeOverview(),
      isError: false,
    });
    mockUseOperatorSnapshot.mockReturnValue({
      data: makeSnapshot(),
      isError: false,
    });

    wrap(<Overview />);

    expect(screen.getByText(/all systems nominal/i)).toBeInTheDocument();
    expect(screen.queryByText(/connection lost/i)).not.toBeInTheDocument();
  });

  it("links Docs to the repository documentation", () => {
    mockUseOverview.mockReturnValue({
      data: makeOverview(),
      isError: false,
    });
    mockUseOperatorSnapshot.mockReturnValue({
      data: makeSnapshot(),
      isError: false,
    });

    wrap(<Overview />);

    expect(screen.getByRole("link", { name: "Docs" })).toHaveAttribute("href", DOCS_URL);
  });

  it("renders agent token usage bars", () => {
    const overview = makeOverview();
    overview.kpi.tokens_24h = 1_508_000;
    overview.agent_tokens = [
      { agent: "codex", tokens_24h: 1_200_000 },
      { agent: "claude", tokens_24h: 308_000 },
    ];
    mockUseOverview.mockReturnValue({
      data: overview,
      isError: false,
    });
    mockUseOperatorSnapshot.mockReturnValue({
      data: makeSnapshot(),
      isError: false,
    });

    wrap(<Overview />);

    expect(screen.getByText("Agent usage")).toBeInTheDocument();
    expect(screen.getByText("codex")).toBeInTheDocument();
    expect(screen.getByText("claude")).toBeInTheDocument();
    expect(screen.getByText("1.2M")).toBeInTheDocument();
    expect(screen.getByText("308K")).toBeInTheDocument();
  });
});
