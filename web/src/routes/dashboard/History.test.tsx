import { beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { fireEvent, render, screen } from "@testing-library/react";
import { History } from "./History";

vi.mock("@/lib/queries", () => ({
  useTasks: vi.fn(),
  useTaskDetail: vi.fn(),
  useTaskArtifacts: vi.fn(),
  useTaskPrompts: vi.fn(),
}));

import { useTaskArtifacts, useTaskDetail, useTaskPrompts, useTasks } from "@/lib/queries";

const mockUseTasks = useTasks as ReturnType<typeof vi.fn>;
const mockUseTaskDetail = useTaskDetail as ReturnType<typeof vi.fn>;
const mockUseTaskArtifacts = useTaskArtifacts as ReturnType<typeof vi.fn>;
const mockUseTaskPrompts = useTaskPrompts as ReturnType<typeof vi.fn>;

function renderHistory(selectedTaskId: string | null, onSelectTask = vi.fn()) {
  return render(
    <MemoryRouter>
      <History selectedTaskId={selectedTaskId} onSelectTask={onSelectTask} />
    </MemoryRouter>,
  );
}

describe("<History>", () => {
  beforeEach(() => {
    mockUseTasks.mockReturnValue({
      data: [{ id: "task-1", status: "done", description: "Ship docs", project: "/srv/repos/harness", pr_url: null }],
      isLoading: false,
      isError: false,
    });
    mockUseTaskArtifacts.mockReturnValue({ data: [] });
    mockUseTaskPrompts.mockReturnValue({ data: [] });
    mockUseTaskDetail.mockReturnValue({
      isLoading: false,
      data: {
        id: "task-1",
        status: "done",
        turn: 1,
        pr_url: null,
        error: null,
        source: null,
        parent_id: null,
        external_id: null,
        repo: null,
        description: "Ship docs",
        created_at: null,
        phase: "terminal",
        depends_on: [],
        subtask_ids: [],
        project: "/srv/repos/harness",
        rounds: [],
        prompts: [],
        artifacts: [],
        completion: {
          is_terminal: true,
          status: "done",
          has_pr: false,
          has_artifacts: false,
          has_prompts: false,
          pr_url: null,
          latest_artifact_at: null,
          latest_prompt_at: null,
          checkpoint: null,
        },
        workflow: null,
      },
    });
  });

  it("reopens completed tasks through the shared detail panel", () => {
    const onSelectTask = vi.fn();
    renderHistory(null, onSelectTask);

    fireEvent.click(screen.getByRole("button", { name: /ship docs/i }));
    expect(onSelectTask).toHaveBeenCalledWith("task-1");
  });

  it("renders explicit not-available states when completion evidence is missing", () => {
    renderHistory("task-1");
    expect(screen.getAllByText("Not available yet").length).toBeGreaterThan(0);
  });
});
