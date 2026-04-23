import { beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { Submit } from "./Submit";

const mockNavigate = vi.fn();

vi.mock("react-router-dom", async (importOriginal) => {
  const actual = await importOriginal<typeof import("react-router-dom")>();
  return { ...actual, useNavigate: () => mockNavigate };
});

vi.mock("@/lib/queries", () => ({
  useDashboard: vi.fn(),
  useProjects: vi.fn(),
  useValidateProject: vi.fn(),
  useRegisterProject: vi.fn(),
  useCreateTask: vi.fn(),
}));

import {
  useCreateTask,
  useDashboard,
  useProjects,
  useRegisterProject,
  useValidateProject,
} from "@/lib/queries";

const mockUseDashboard = useDashboard as ReturnType<typeof vi.fn>;
const mockUseProjects = useProjects as ReturnType<typeof vi.fn>;
const mockUseValidateProject = useValidateProject as ReturnType<typeof vi.fn>;
const mockUseRegisterProject = useRegisterProject as ReturnType<typeof vi.fn>;
const mockUseCreateTask = useCreateTask as ReturnType<typeof vi.fn>;

function renderSubmit() {
  return render(
    <MemoryRouter>
      <Submit />
    </MemoryRouter>,
  );
}

describe("<Submit>", () => {
  beforeEach(() => {
    mockNavigate.mockReset();
    mockUseDashboard.mockReturnValue({ data: { onboarding: { phase: "submit_task" } } });
    mockUseProjects.mockReturnValue({
      data: [{ id: "project-1", root: "/srv/repos/harness", name: "harness" }],
    });
    mockUseValidateProject.mockReturnValue({
      isPending: false,
      data: undefined,
      variables: undefined,
      mutateAsync: vi.fn(),
    });
    mockUseRegisterProject.mockReturnValue({
      isPending: false,
      mutateAsync: vi.fn(),
    });
    mockUseCreateTask.mockReturnValue({
      isPending: false,
      mutateAsync: vi.fn().mockResolvedValue({
        task_id: "task-1",
        status: "running",
        deduped: false,
        task_url: "/?task=task-1",
      }),
    });
  });

  it("renders a project picker when projects exist", () => {
    renderSubmit();
    expect(screen.getByDisplayValue("harness")).toBeInTheDocument();
  });

  it("applies starter templates to the form", () => {
    renderSubmit();
    fireEvent.click(screen.getByRole("button", { name: "Polish README" }));
    expect(screen.getByDisplayValue("Improve the onboarding docs")).toBeInTheDocument();
    expect(screen.getByDisplayValue(/first-run documentation/i)).toBeInTheDocument();
  });

  it("renders validation failures from the server", async () => {
    const mutateAsync = vi.fn().mockRejectedValue(new Error("root is not a git repository"));
    mockUseValidateProject.mockReturnValue({
      isPending: false,
      data: undefined,
      variables: undefined,
      mutateAsync,
    });

    renderSubmit();
    fireEvent.change(screen.getByPlaceholderText("/absolute/path/to/your/repository"), {
      target: { value: "/tmp/not-a-repo" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Validate" }));

    await waitFor(() =>
      expect(screen.getByText("root is not a git repository")).toBeInTheDocument(),
    );
  });

  it("submits a task and redirects into the selected task", async () => {
    const mutateAsync = vi.fn().mockResolvedValue({
      task_id: "task-7",
      status: "running",
      deduped: false,
      task_url: "/?task=task-7",
    });
    mockUseCreateTask.mockReturnValue({
      isPending: false,
      mutateAsync,
    });

    renderSubmit();
    fireEvent.change(screen.getByLabelText("Title"), {
      target: { value: "Investigate a failure" },
    });
    fireEvent.change(screen.getByLabelText("Description"), {
      target: { value: "Find the root cause and add coverage." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Submit task" }));

    await waitFor(() => expect(mutateAsync).toHaveBeenCalledOnce());
    expect(mockNavigate).toHaveBeenCalledWith("/?task=task-7");
  });
});
