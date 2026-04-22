import { describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import { Overview } from "./Overview";
import { PaletteProvider } from "@/lib/palette";
import type { OperatorSnapshotPayload, OverviewPayload } from "@/types";

vi.mock("@/lib/queries", () => ({
  useOverview: vi.fn(),
  useOperatorSnapshot: vi.fn(),
}));

import { useOperatorSnapshot, useOverview } from "@/lib/queries";

const mockUseOverview = useOverview as ReturnType<typeof vi.fn>;
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
    distribution: {
      queued: 0,
      running: 1,
      review: 0,
      merged: 0,
      failed: 0,
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
    global: {
      uptime_secs: 1,
      running: 1,
      queued: 0,
      done: 0,
      failed: 0,
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
  };
}

describe("<Overview>", () => {
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
});
