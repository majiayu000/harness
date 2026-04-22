import { describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import { OperatorPanel } from "./OperatorPanel";
import type { OperatorSnapshotPayload } from "@/types";

vi.mock("@/lib/queries", () => ({
  useOperatorSnapshot: vi.fn(),
}));

import { useOperatorSnapshot } from "@/lib/queries";

const mockUseOperatorSnapshot = useOperatorSnapshot as ReturnType<typeof vi.fn>;

function wrap(ui: React.ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
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

describe("<OperatorPanel>", () => {
  it("shows an unavailable message instead of healthy-empty copy when operator snapshot query fails", () => {
    mockUseOperatorSnapshot.mockReturnValue({
      data: undefined,
      isError: true,
    });

    wrap(<OperatorPanel />);

    expect(screen.getAllByText("Operator snapshot unavailable.").length).toBe(2);
    expect(screen.queryByText("No retry ticks recorded yet.")).not.toBeInTheDocument();
    expect(screen.queryByText("No recent failures.")).not.toBeInTheDocument();
  });

  it("still shows the empty-state copy when the operator snapshot query succeeds with no data yet", () => {
    mockUseOperatorSnapshot.mockReturnValue({
      data: makeSnapshot(),
      isError: false,
    });

    wrap(<OperatorPanel />);

    expect(screen.getByText("No retry ticks recorded yet.")).toBeInTheDocument();
    expect(screen.getByText("No recent failures.")).toBeInTheDocument();
    expect(screen.queryByText("Operator snapshot unavailable.")).not.toBeInTheDocument();
  });
});
