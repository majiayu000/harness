import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { TaskSlideover } from "./TaskSlideover";
import type { TaskDetail, TaskPromptRecord } from "@/types";

vi.mock("@/lib/queries", () => ({
  useTaskDetail: vi.fn(),
  useTaskPrompts: vi.fn(),
}));

import { useTaskDetail, useTaskPrompts } from "@/lib/queries";

const mockUseTaskDetail = useTaskDetail as ReturnType<typeof vi.fn>;
const mockUseTaskPrompts = useTaskPrompts as ReturnType<typeof vi.fn>;

const detail: TaskDetail = {
  id: "task-12345678",
  status: "running",
  turn: 2,
  pr_url: null,
  error: null,
  source: "dashboard",
  parent_id: null,
  external_id: null,
  repo: "majiayu000/harness",
  description: "Prompt task",
  created_at: "2026-04-22T13:00:00Z",
  phase: "implement",
  depends_on: [],
  subtask_ids: [],
  project: "/tmp/harness",
  task_kind: "implementation",
  rounds: [
    { turn: 1, action: "plan", result: "created implementation plan" },
    { turn: 2, action: "implement", result: "editing files" },
  ],
};

function makePrompt(turn: number, prompt: string, createdAt: string): TaskPromptRecord {
  return {
    task_id: "task-12345678",
    turn,
    phase: turn === 1 ? "plan" : "implement",
    prompt,
    created_at: createdAt,
  };
}

describe("<TaskSlideover>", () => {
  beforeEach(() => {
    mockUseTaskDetail.mockReturnValue({
      data: detail,
      isLoading: false,
      isError: false,
    });
    mockUseTaskPrompts.mockReturnValue({
      data: [],
      isLoading: false,
      isError: false,
    });
  });

  it("renders prompts in order and keeps long prompt text fully accessible", () => {
    const longPrompt = `${"line of prompt context\n".repeat(80)}THE-END`;
    mockUseTaskPrompts.mockReturnValue({
      data: [
        makePrompt(1, "planner prompt", "2026-04-22T13:00:01Z"),
        makePrompt(2, longPrompt, "2026-04-22T13:00:02Z"),
      ],
      isLoading: false,
      isError: false,
    });

    render(<TaskSlideover taskId="task-12345678" open onClose={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: "Prompts" }));

    const prompts = screen.getAllByTestId("task-prompt");
    expect(prompts).toHaveLength(2);
    expect(prompts[0]).toHaveTextContent("planner prompt");
    const promptBody = screen.getByLabelText("Prompt body turn 2");
    expect(promptBody.textContent).toBe(longPrompt);
    expect(promptBody.className).toContain("overflow-auto");
  });

  it("renders loading, empty, and error states for prompts", () => {
    const { rerender } = render(<TaskSlideover taskId="task-12345678" open onClose={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: "Prompts" }));
    expect(screen.getByText("No prompts recorded for this task.")).toBeInTheDocument();

    mockUseTaskPrompts.mockReturnValue({
      data: undefined,
      isLoading: true,
      isError: false,
    });
    rerender(<TaskSlideover taskId="task-12345678" open onClose={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: "Prompts" }));
    expect(screen.getByText("Loading prompts…")).toBeInTheDocument();

    mockUseTaskPrompts.mockReturnValue({
      data: undefined,
      isLoading: false,
      isError: true,
    });
    rerender(<TaskSlideover taskId="task-12345678" open onClose={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: "Prompts" }));
    expect(screen.getByText("Unable to load prompts.")).toBeInTheDocument();
  });

  it("closes on backdrop click and Escape", () => {
    const onClose = vi.fn();
    render(<TaskSlideover taskId="task-12345678" open onClose={onClose} />);

    fireEvent.click(screen.getByLabelText("Close task detail"));
    fireEvent.keyDown(window, { key: "Escape" });

    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it("does not emit duplicate key warnings for repeated round actions", () => {
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
    mockUseTaskDetail.mockReturnValue({
      data: {
        ...detail,
        rounds: [
          { turn: 2, action: "transient_retry", result: "retrying after transient failure" },
          { turn: 2, action: "transient_retry", result: "retry succeeded" },
        ],
      },
      isLoading: false,
      isError: false,
    });

    render(<TaskSlideover taskId="task-12345678" open onClose={() => {}} />);

    expect(screen.getAllByText(/transient_retry/)).toHaveLength(2);
    expect(
      consoleError.mock.calls.some(([message]) =>
        String(message).includes("Each child in a list should have a unique"),
      ),
    ).toBe(false);

    consoleError.mockRestore();
  });
});
