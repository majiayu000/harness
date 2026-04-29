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

vi.mock("@/lib/api", () => ({
  apiJson: vi.fn().mockResolvedValue([]),
}));

import { useTaskDetail, useTaskStream } from "@/lib/queries";
import { apiJson } from "@/lib/api";

const mockUseTaskDetail = useTaskDetail as ReturnType<typeof vi.fn>;
const mockUseTaskStream = useTaskStream as ReturnType<typeof vi.fn>;
const mockApiJson = apiJson as ReturnType<typeof vi.fn>;

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
  mockApiJson.mockResolvedValue([]);
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
    const task = makeFullTask({
      status: "implementing",
      repo: "owner/repo",
      description: "Fix the bug",
      workflow: {
        state: "ready_to_merge",
        pr_number: 42,
        review_fallback: {
          tier: "c",
          trigger: "all_bots_quota",
          active_bot: "codex",
          activated_at: "2026-04-30T00:00:00Z",
        },
      },
    });
    mockUseTaskDetail.mockReturnValue({ data: task, isLoading: false, isError: false });
    wrap(<TaskDetailSlideover taskId="task-abc-123" onClose={vi.fn()} />);
    // "implementing" appears in both the header badge and the summary field
    expect(screen.getAllByText("implementing").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("owner/repo")).toBeInTheDocument();
    expect(screen.getByText("Fix the bug")).toBeInTheDocument();
    expect(screen.getByText("C")).toBeInTheDocument();
    expect(screen.getByText("all bots quota")).toBeInTheDocument();
    expect(screen.getByText("codex")).toBeInTheDocument();
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

  // ── proof-of-work card ────────────────────────────────────────────────────

  it("renders proof-of-work card for terminal task", async () => {
    const task = makeFullTask({
      status: "done",
      pr_url: "https://github.com/owner/repo/pull/42",
      workflow: { state: "ready_to_merge", pr_number: 42 },
      updated_at: "2024-01-01T01:00:00Z",
    });
    mockUseTaskDetail.mockReturnValue({ data: task, isLoading: false, isError: false });
    mockApiJson.mockImplementation((url: string) => {
      if (url.includes("/artifacts")) {
        return Promise.resolve([
          { task_id: task.id, turn: 1, artifact_type: "patch", content: "diff --git a/x", created_at: "2024-01-01T00:30:00Z" },
        ]);
      }
      if (url.includes("/prompts")) {
        return Promise.resolve([
          { task_id: task.id, turn: 1, phase: "implement", prompt: "Write the fix", created_at: "2024-01-01T00:10:00Z" },
        ]);
      }
      return Promise.resolve([]);
    });
    wrap(<TaskDetailSlideover taskId={task.id} onClose={vi.fn()} />);
    const card = await screen.findByTestId("proof-of-work-card");
    expect(card).toBeInTheDocument();
    expect(card).toHaveTextContent("https://github.com/owner/repo/pull/42");
    expect(card).toHaveTextContent("Rounds: 2");
    expect(card).toHaveTextContent("Approved");
    expect(await screen.findByText("patch")).toBeInTheDocument();
    expect(await screen.findByText(/implement/)).toBeInTheDocument();
    expect(card).toHaveTextContent("1 prompt recorded");
    expect(card).not.toHaveTextContent("Write the fix");
  });

  it("does not render proof-of-work card for non-terminal task", () => {
    const task = makeFullTask({ status: "implementing" });
    mockUseTaskDetail.mockReturnValue({ data: task, isLoading: false, isError: false });
    wrap(<TaskDetailSlideover taskId={task.id} onClose={vi.fn()} />);
    expect(screen.queryByTestId("proof-of-work-card")).not.toBeInTheDocument();
  });

  it("proof-of-work card degrades gracefully with missing data", async () => {
    const task = makeFullTask({ status: "done", pr_url: null, workflow: null });
    mockUseTaskDetail.mockReturnValue({ data: task, isLoading: false, isError: false });
    wrap(<TaskDetailSlideover taskId={task.id} onClose={vi.fn()} />);
    const card = await screen.findByTestId("proof-of-work-card");
    expect(card).toHaveTextContent("No PR");
    expect(card).toHaveTextContent("No reviewer data");
    await waitFor(() => {
      expect(card).toHaveTextContent("No prompts recorded");
      expect(card).toHaveTextContent("No artifacts");
    });
  });

  it("fetches and renders prompt history in chronological order for the prompts tab", async () => {
    const task = makeFullTask({ status: "implementing" });
    mockUseTaskDetail.mockReturnValue({ data: task, isLoading: false, isError: false });
    mockApiJson.mockImplementation((url: string) => {
      if (url.includes("/prompts")) {
        return Promise.resolve([
          {
            task_id: task.id,
            turn: 2,
            phase: "retry",
            prompt: "Second prompt body",
            created_at: "2024-01-01T00:20:00Z",
          },
          {
            task_id: task.id,
            turn: 1,
            phase: "plan",
            prompt: "First prompt body",
            created_at: "2024-01-01T00:10:00Z",
          },
        ]);
      }
      return Promise.resolve([]);
    });

    wrap(<TaskDetailSlideover taskId={task.id} onClose={vi.fn()} />);
    fireEvent.click(screen.getByText("prompts"));

    expect(await screen.findByText("First prompt body")).toBeInTheDocument();
    expect(screen.getByText("Second prompt body")).toBeInTheDocument();

    const promptBodies = await screen.findAllByTestId(/prompt-body-/);
    expect(promptBodies[0]).toHaveTextContent("First prompt body");
    expect(promptBodies[1]).toHaveTextContent("Second prompt body");
    expect(screen.getByText("plan")).toBeInTheDocument();
    expect(screen.getByText("retry")).toBeInTheDocument();
    expect(screen.getByText("2024-01-01T00:10:00Z")).toBeInTheDocument();
    expect(screen.getByText("2024-01-01T00:20:00Z")).toBeInTheDocument();
  });

  it("renders long multiline prompts in full without truncation", async () => {
    const task = makeFullTask({ status: "implementing" });
    const longPrompt = ["Line 1", "Line 2", "Line 3", "x".repeat(700)].join("\n");
    mockUseTaskDetail.mockReturnValue({ data: task, isLoading: false, isError: false });
    mockApiJson.mockImplementation((url: string) => {
      if (url.includes("/prompts")) {
        return Promise.resolve([
          {
            task_id: task.id,
            turn: 1,
            phase: "implement",
            prompt: longPrompt,
            created_at: "2024-01-01T00:10:00Z",
          },
        ]);
      }
      return Promise.resolve([]);
    });

    wrap(<TaskDetailSlideover taskId={task.id} onClose={vi.fn()} />);
    fireEvent.click(screen.getByText("prompts"));

    const promptBody = await screen.findByTestId("prompt-body-0");
    expect(promptBody.textContent).toBe(longPrompt);
    expect(promptBody).toHaveTextContent("Line 1");
    expect(promptBody).toHaveTextContent("Line 2");
    expect(promptBody).toHaveTextContent("Line 3");
    expect(promptBody.textContent?.endsWith("…")).toBe(false);
  });

  it("renders a clear empty state when no prompts are recorded", async () => {
    const task = makeFullTask({ status: "implementing" });
    mockUseTaskDetail.mockReturnValue({ data: task, isLoading: false, isError: false });
    mockApiJson.mockImplementation((url: string) => {
      if (url.includes("/prompts")) {
        return Promise.resolve([]);
      }
      return Promise.resolve([]);
    });

    wrap(<TaskDetailSlideover taskId={task.id} onClose={vi.fn()} />);
    fireEvent.click(screen.getByText("prompts"));

    expect(await screen.findByText("No prompts recorded.")).toBeInTheDocument();
  });

  it("renders a clear error state when prompts fail to load", async () => {
    const task = makeFullTask({ status: "implementing" });
    mockUseTaskDetail.mockReturnValue({ data: task, isLoading: false, isError: false });
    mockApiJson.mockImplementation((url: string) => {
      if (url.includes("/prompts")) {
        return Promise.reject(new Error("boom"));
      }
      return Promise.resolve([]);
    });

    wrap(<TaskDetailSlideover taskId={task.id} onClose={vi.fn()} />);
    fireEvent.click(screen.getByText("prompts"));

    expect(await screen.findByRole("alert")).toHaveTextContent("Failed to load prompts.");
  });

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
