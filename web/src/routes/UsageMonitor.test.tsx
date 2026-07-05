import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { PaletteProvider } from "@/lib/palette";
import type { UsageMonitorPayload } from "@/types";
import { UsageMonitor } from "./UsageMonitor";

vi.mock("@/lib/queries", () => ({
  useUsageMonitor: vi.fn(),
}));

import { useUsageMonitor } from "@/lib/queries";

const mockUseUsageMonitor = useUsageMonitor as ReturnType<typeof vi.fn>;

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

function makePayload(): UsageMonitorPayload {
  return {
    window: {
      hours: 24,
      since: "2026-07-04T00:00:00Z",
      now: "2026-07-05T00:00:00Z",
    },
    cost: {
      currency: "USD",
      configured: false,
      source: "HARNESS_USAGE_PRICE_CATALOG_JSON",
      missing_model_count: 0,
      message: "price catalog unavailable",
    },
    summary: {
      total_tokens: 0,
      input_tokens: 0,
      output_tokens: 0,
      cache_read_input_tokens: 0,
      cache_creation_input_tokens: 0,
      request_count: 0,
      estimated_cost_usd: null,
      running_agent_invocations: 0,
      active_leased_agent_invocations: 0,
      expired_or_missing_lease_agent_invocations: 0,
      pending_agent_invocations: 0,
      stale_agent_invocations: 0,
      high_burn_invocations: 0,
      external_agent_processes: 0,
    },
    tokens_by_agent: [],
    tokens_by_project: [],
    tokens_by_model: [],
    agent_invocations: [],
    external_agent_processes: [],
    local_usage_sources: [],
    active_by_repo: [],
    active_by_activity: [],
    postgres_catalog: {
      state: "available",
      schema_count: 16_980,
      catalog_object_count: 172_043,
      database_size_bytes: 5_900_000_000,
      sampled_at: "2026-07-05T00:00:00Z",
      startup_schema_count: 16_000,
      absolute_schema_threshold: 50_000,
      relative_schema_multiplier: 3,
      threshold_breached: false,
      breach_reasons: [],
      error: null,
    },
    diagnostics: {
      runtime_store_available: true,
      token_source: "workflow_runtime_usage_and_llm_usage_events",
      active_cost_confidence: "estimated_from_agent_invocation_state",
      process_source: "external_cli_process_snapshot",
    },
  };
}

describe("<UsageMonitor>", () => {
  beforeEach(() => {
    mockUseUsageMonitor.mockReset();
  });

  it("renders Postgres catalog counts and breached state", () => {
    const payload = makePayload();
    payload.postgres_catalog.threshold_breached = true;
    payload.postgres_catalog.breach_reasons = ["schema_count_gt_absolute_threshold"];
    mockUseUsageMonitor.mockReturnValue({ data: payload, isError: false });

    wrap(<UsageMonitor />);

    expect(screen.getByText("Postgres catalog")).toBeInTheDocument();
    expect(screen.getByText("16,980")).toBeInTheDocument();
    expect(screen.getByText("172,043")).toBeInTheDocument();
    expect(screen.getByText("5.5GB")).toBeInTheDocument();
    expect(screen.getByText("breached")).toBeInTheDocument();
    expect(screen.getByText("schema_count_gt_absolute_threshold")).toBeInTheDocument();
  });

  it("renders Postgres catalog unavailable state", () => {
    const payload = makePayload();
    payload.postgres_catalog = {
      state: "unavailable",
      schema_count: null,
      catalog_object_count: null,
      database_size_bytes: null,
      sampled_at: null,
      startup_schema_count: null,
      absolute_schema_threshold: 50_000,
      relative_schema_multiplier: 3,
      threshold_breached: false,
      breach_reasons: [],
      error: "postgres_pool_unavailable",
    };
    mockUseUsageMonitor.mockReturnValue({ data: payload, isError: false });

    wrap(<UsageMonitor />);

    expect(screen.getByText("postgres_pool_unavailable")).toBeInTheDocument();
    expect(screen.getByText("unavailable")).toBeInTheDocument();
  });
});
