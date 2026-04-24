import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { TaskDetailSlideover } from "./TaskDetailSlideover";
import type { FullTask } from "@/types";

vi.mock("@/lib/queries", () => ({
  useTaskDetail: vi.fn(),
  useTaskStream: vi.fn(),
}));

import { useTaskDetail, useTaskStream } from "@/lib/queries";

const mockUseTaskDetail = useTaskDetail as ReturnType<typeof vi.fn>;
const mockUseTaskStream = useTaskStream as ReturnType<typeof vi.fn>;

function makeFullTask(overrides: Partial<FullTask> = {}): FullTask {
  return {
    id: "task-abc-123",
    task_kind: "issue",
    status: "implementing",
    turn: 2,
    pr_url: null,
    error: null,
    source: null,
    parent_id: null,
    external_id: null,
    repo: "owner/repo",
    description: "Fix the bug",
    created_at: "2024-01-01T00:00:00Z",
    phase: "coding",
    depends_on: [],
    subtask_ids: [],
    project: null,
    workflow: null,
    rounds: [],
    system_input: null,
    request_settings: null,
    ...overrides,
  };
}

function wrap(ui: React.ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

beforeEach(() => {
  mockUseTaskStream.mockImplementation(() => undefined);
  mockUseTaskDetail.mockReturnValue({ data: undefined, isLoading: false, isError: false });
});

// ── visibility ────────────────────────────────────────────────────────────────

describe("TaskDetailSlideover", () => {
  it("renders nothing when taskId is null", () => {
    const { container } = wrap(
      <TaskDetailSlideover taskId={null} onClose={vi.fn()} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders the panel when taskId is set", () => {
    wrap(<TaskDetailSlideover taskId="task-abc-123" onClose={vi.fn()} />);
    expect(screen.getByText("task-abc")).toBeInTheDocument();
  });

  // ── loading / error states ────────────────────────────────────────────────

  it("shows loading indicator while useTaskDetail is loading", () => {
    mockUseTaskDetail.mockReturnValue({ data: undefined, isLoading: true, isError: false });
    wrap(<TaskDetailSlideover taskId="task-abc-123" onClose={vi.fn()} />);
    expect(screen.getByRole("status")).toHaveTextContent("Loading");
  });

  it("shows error message when useTaskDetail returns an error", () => {
    mockUseTaskDetail.mockReturnValue({ data: undefined, isLoading: false, isError: true });
    wrap(<TaskDetailSlideover taskId="task-abc-123" onClose={vi.fn()} />);
    expect(screen.getByRole("alert")).toHaveTextContent("Failed to load task");
  });

  // ── summary tab ───────────────────────────────────────────────────────────

  it("renders task summary fields when data resolves", () => {
    const task = makeFullTask({ status: "implementing", repo: "owner/repo", description: "Fix the bug" });
    mockUseTaskDetail.mockReturnValue({ data: task, isLoading: false, isError: false });
    wrap(<TaskDetailSlideover taskId="task-abc-123" onClose={vi.fn()} />);
    // "implementing" appears in both the header badge and the summary field
    expect(screen.getAllByText("implementing").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("owner/repo")).toBeInTheDocument();
    expect(screen.getByText("Fix the bug")).toBeInTheDocument();
  });

  // ── SSE output tab ────────────────────────────────────────────────────────

  it("appends SSE stream chunks to the Output tab", async () => {
    let capturedCallback: ((text: string) => void) | null = null;
    mockUseTaskStream.mockImplementation((_id: unknown, onChunk: (text: string) => void) => {
      capturedCallback = onChunk;
    });
    wrap(<TaskDetailSlideover taskId="task-abc-123" onClose={vi.fn()} />);

    fireEvent.click(screen.getByText("output"));

    // Simulate stream chunk arriving
    capturedCallback!("hello ");
    capturedCallback!("world");

    await waitFor(() => {
      expect(screen.getByText(/hello world/)).toBeInTheDocument();
    });
  });

  // ── close behaviour ───────────────────────────────────────────────────────

  it("calls onClose when the scrim is clicked", () => {
    const onClose = vi.fn();
    wrap(<TaskDetailSlideover taskId="task-abc-123" onClose={onClose} />);
    fireEvent.click(screen.getByTestId("slideover-scrim"));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("calls onClose when the close button is clicked", () => {
    const onClose = vi.fn();
    wrap(<TaskDetailSlideover taskId="task-abc-123" onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("calls onClose when Escape is pressed", () => {
    const onClose = vi.fn();
    wrap(<TaskDetailSlideover taskId="task-abc-123" onClose={onClose} />);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  // ── tab switching ─────────────────────────────────────────────────────────

  it("switches tabs without remounting (Summary → Output → Summary)", () => {
    const task = makeFullTask();
    mockUseTaskDetail.mockReturnValue({ data: task, isLoading: false, isError: false });
    wrap(<TaskDetailSlideover taskId="task-abc-123" onClose={vi.fn()} />);

    expect(screen.getByText("issue")).toBeInTheDocument(); // summary visible

    fireEvent.click(screen.getByText("output"));
    expect(screen.queryByText("issue")).not.toBeInTheDocument();

    fireEvent.click(screen.getByText("summary"));
    expect(screen.getByText("issue")).toBeInTheDocument();
  });
});
