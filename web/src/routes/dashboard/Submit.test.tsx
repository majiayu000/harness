import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { Submit } from "./Submit";
import { SubmitSuccess } from "./SubmitSuccess";
import { useOverview, useTaskStream, useCancelTask, useCancelWorkflowRuntime } from "@/lib/queries";
import { apiFetch } from "@/lib/api";

// ── Mocks ──────────────────────────────────────────────────────────────────────

vi.mock("@/lib/queries", () => ({
  useOverview: vi.fn(),
  useTaskStream: vi.fn(),
  useCancelTask: vi.fn(),
  useCancelWorkflowRuntime: vi.fn(),
}));

vi.mock("@/lib/api", () => ({
  apiFetch: vi.fn(),
  TOKEN_KEY: "harness_token",
  ApiError: class ApiError extends Error {
    constructor(
      public status: number,
      message: string,
    ) {
      super(message);
    }
  },
}));

const mockUseOverview = useOverview as ReturnType<typeof vi.fn>;
const mockUseTaskStream = useTaskStream as ReturnType<typeof vi.fn>;
const mockUseCancelTask = useCancelTask as ReturnType<typeof vi.fn>;
const mockUseCancelWorkflowRuntime = useCancelWorkflowRuntime as ReturnType<typeof vi.fn>;
const mockApiFetch = apiFetch as ReturnType<typeof vi.fn>;

// ── Helpers ────────────────────────────────────────────────────────────────────

function makeQC() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

function wrap(ui: React.ReactElement) {
  return render(<QueryClientProvider client={makeQC()}>{ui}</QueryClientProvider>);
}

const PROJECTS = [
  { id: "alpha", root: "/alpha", agents: ["claude", "codex"], running: 0, queued: 0, done: 0, failed: 0, merged_24h: 0, trend: [], avg_score: null, worktrees: null, tokens_24h: null, latest_pr: null },
  { id: "beta", root: "/beta", agents: ["gemini"], running: 0, queued: 0, done: 0, failed: 0, merged_24h: 0, trend: [], avg_score: null, worktrees: null, tokens_24h: null, latest_pr: null },
];

beforeEach(() => {
  mockUseOverview.mockReturnValue({ data: { projects: PROJECTS } });
  mockUseTaskStream.mockImplementation(() => {});
  mockUseCancelTask.mockReturnValue({ mutate: vi.fn(), isPending: false });
  mockUseCancelWorkflowRuntime.mockReturnValue({ mutate: vi.fn(), isPending: false });
  mockApiFetch.mockResolvedValue(
    new Response(JSON.stringify({ task_id: "task-xyz", status: "running" }), { status: 200 }),
  );
});

afterEach(() => {
  vi.restoreAllMocks();
});

// Navigate the wizard to the config step with the given mode
function goToConfig(mode: "Issue" | "Pull Request" | "Prompt") {
  fireEvent.click(screen.getByRole("button", { name: mode }));
  fireEvent.click(screen.getByRole("button", { name: /Next/ }));
}

// ── <Submit> tests ─────────────────────────────────────────────────────────────

describe("<Submit>", () => {
  it("step 1: renders three mode options with none pre-selected", () => {
    wrap(<Submit />);
    expect(screen.getByRole("button", { name: "Issue" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Pull Request" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Prompt" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Next/ })).toBeDisabled();
  });

  it("selecting issue shows only the issue field; selecting pr shows only the pr field", () => {
    wrap(<Submit />);

    // issue mode → config
    goToConfig("Issue");
    expect(screen.getByPlaceholderText(/123 or/)).toBeInTheDocument();
    expect(screen.queryByPlaceholderText(/456 or/)).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: /Prompt/ })).not.toBeInTheDocument();

    // back to mode, switch to pr
    fireEvent.click(screen.getByRole("button", { name: /Back/ }));
    goToConfig("Pull Request");
    expect(screen.getByPlaceholderText(/456 or/)).toBeInTheDocument();
    expect(screen.queryByPlaceholderText(/123 or/)).not.toBeInTheDocument();
  });

  it("Next is disabled in config step until required field is non-empty", () => {
    wrap(<Submit />);
    goToConfig("Issue");

    let nextBtn = screen.getByRole("button", { name: /Next/ });
    expect(nextBtn).toBeDisabled();

    // invalid input (not a number or URL)
    fireEvent.change(screen.getByPlaceholderText(/123 or/), { target: { value: "not-a-number" } });
    expect(nextBtn).toBeDisabled();

    // valid issue number still needs a project when projects are registered
    fireEvent.change(screen.getByPlaceholderText(/123 or/), { target: { value: "42" } });
    expect(nextBtn).toBeDisabled();

    fireEvent.change(screen.getByRole("combobox", { name: /Project/ }), {
      target: { value: "alpha" },
    });
    expect(nextBtn).not.toBeDisabled();
  });

  it("rejects non-positive issue and PR identifiers", () => {
    wrap(<Submit />);
    goToConfig("Issue");

    let nextBtn = screen.getByRole("button", { name: /Next/ });
    fireEvent.change(screen.getByRole("combobox", { name: /Project/ }), {
      target: { value: "alpha" },
    });

    fireEvent.change(screen.getByPlaceholderText(/123 or/), { target: { value: "0" } });
    expect(nextBtn).toBeDisabled();

    fireEvent.change(screen.getByPlaceholderText(/123 or/), {
      target: { value: "https://github.com/owner/repo/issues/0" },
    });
    expect(nextBtn).toBeDisabled();

    fireEvent.click(screen.getByRole("button", { name: /Back/ }));
    goToConfig("Pull Request");
    nextBtn = screen.getByRole("button", { name: /Next/ });
    fireEvent.change(screen.getByRole("combobox", { name: /Project/ }), {
      target: { value: "alpha" },
    });

    fireEvent.change(screen.getByPlaceholderText(/456 or/), { target: { value: "-1" } });
    expect(nextBtn).toBeDisabled();

    fireEvent.change(screen.getByPlaceholderText(/456 or/), { target: { value: "7" } });
    expect(nextBtn).not.toBeDisabled();
  });

  it("project dropdown lists registered project names from useOverview", () => {
    wrap(<Submit />);
    goToConfig("Issue");

    expect(screen.getByRole("option", { name: "alpha" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "beta" })).toBeInTheDocument();
  });

  it("blocks config advance while the project list is loading", () => {
    mockUseOverview.mockReturnValue({ data: undefined, isLoading: true });
    wrap(<Submit />);

    goToConfig("Prompt");
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "do something" } });

    expect(screen.getByText("Loading projects…")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Next/ })).toBeDisabled();
  });

  it("agent dropdown populates from the selected project's agents array", () => {
    wrap(<Submit />);
    goToConfig("Issue");

    fireEvent.change(screen.getByRole("combobox", { name: /Project/ }), {
      target: { value: "alpha" },
    });
    expect(screen.getByRole("option", { name: "claude" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "codex" })).toBeInTheDocument();

    fireEvent.change(screen.getByRole("combobox", { name: /Project/ }), {
      target: { value: "beta" },
    });
    expect(screen.getByRole("option", { name: "gemini" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "claude" })).not.toBeInTheDocument();
  });

  it("submitting an issue task calls POST /tasks with correct payload", async () => {
    wrap(<Submit />);

    goToConfig("Issue");
    fireEvent.change(screen.getByPlaceholderText(/123 or/), { target: { value: "99" } });
    fireEvent.change(screen.getByRole("combobox", { name: /Project/ }), {
      target: { value: "alpha" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Next/ }));

    // options step
    fireEvent.click(screen.getByRole("button", { name: /Submit Task/ }));

    await waitFor(() => expect(mockApiFetch).toHaveBeenCalledWith("/tasks", expect.objectContaining({ method: "POST" })));

    const call = mockApiFetch.mock.calls[0];
    const body = JSON.parse(call[1].body as string);
    expect(body).toMatchObject({ issue: 99, project: "alpha" });
  });

  it("rejects invalid max turns before submitting", async () => {
    wrap(<Submit />);

    goToConfig("Issue");
    fireEvent.change(screen.getByPlaceholderText(/123 or/), { target: { value: "99" } });
    fireEvent.change(screen.getByRole("combobox", { name: /Project/ }), {
      target: { value: "alpha" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Next/ }));

    fireEvent.change(screen.getAllByPlaceholderText(/leave blank for default/)[0], {
      target: { value: "1.5" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Submit Task/ }));

    expect(await screen.findByText("Max turns must be a positive integer")).toBeInTheDocument();
    expect(mockApiFetch).not.toHaveBeenCalled();
  });

  it("accepts zero turn timeout as no timeout and rejects invalid timeout values", async () => {
    wrap(<Submit />);

    goToConfig("Issue");
    fireEvent.change(screen.getByPlaceholderText(/123 or/), { target: { value: "99" } });
    fireEvent.change(screen.getByRole("combobox", { name: /Project/ }), {
      target: { value: "alpha" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Next/ }));

    const timeoutInput = screen.getAllByPlaceholderText(/leave blank for default/)[1];
    fireEvent.change(timeoutInput, { target: { value: "-1" } });
    fireEvent.click(screen.getByRole("button", { name: /Submit Task/ }));

    expect(
      await screen.findByText("Turn timeout seconds must be a non-negative integer"),
    ).toBeInTheDocument();
    expect(mockApiFetch).not.toHaveBeenCalled();

    fireEvent.change(timeoutInput, { target: { value: "0" } });
    fireEvent.click(screen.getByRole("button", { name: /Submit Task/ }));

    await waitFor(() => expect(mockApiFetch).toHaveBeenCalled());
    const body = JSON.parse(mockApiFetch.mock.calls[0][1].body as string);
    expect(body.turn_timeout_secs).toBe(0);
  });

  it("successful submit transitions to the success screen with the returned taskId", async () => {
    wrap(<Submit />);

    goToConfig("Prompt");
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "do something" } });
    fireEvent.change(screen.getByRole("combobox", { name: /Project/ }), {
      target: { value: "alpha" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Next/ }));
    fireEvent.click(screen.getByRole("button", { name: /Submit Task/ }));

    await waitFor(() =>
      expect(screen.getByText("task-xyz")).toBeInTheDocument(),
    );
  });

  it("no projects: shows empty state message and form can still advance to submit", async () => {
    mockUseOverview.mockReturnValue({ data: { projects: [] } });
    wrap(<Submit />);

    goToConfig("Prompt");
    expect(screen.getByText("No projects registered")).toBeInTheDocument();

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "hello" } });
    fireEvent.click(screen.getByRole("button", { name: /Next/ }));
    fireEvent.click(screen.getByRole("button", { name: /Submit Task/ }));

    await waitFor(() => expect(mockApiFetch).toHaveBeenCalled());
    const body = JSON.parse(mockApiFetch.mock.calls[0][1].body as string);
    expect(body.project).toBeUndefined();
    expect(body.prompt).toBe("hello");
  });
});

// ── <SubmitSuccess> tests ──────────────────────────────────────────────────────

describe("<SubmitSuccess>", () => {
  it("cancel button calls useCancelTask.mutate with taskId and triggers onReset", () => {
    const onReset = vi.fn();
    const mockMutate = vi.fn().mockImplementation((_id: string, opts?: { onSuccess?: () => void }) => {
      opts?.onSuccess?.();
    });
    mockUseCancelTask.mockReturnValue({ mutate: mockMutate, isPending: false });

    wrap(<SubmitSuccess taskId="task-abc" onReset={onReset} />);

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(mockMutate).toHaveBeenCalledWith("task-abc", expect.objectContaining({ onSuccess: expect.any(Function) }));
    expect(onReset).toHaveBeenCalled();
  });

  it("runtime cancel uses the workflow runtime endpoint handle", () => {
    const onReset = vi.fn();
    const mockTaskMutate = vi.fn();
    const mockRuntimeMutate = vi.fn().mockImplementation((_id: string, opts?: { onSuccess?: () => void }) => {
      opts?.onSuccess?.();
    });
    mockUseCancelTask.mockReturnValue({ mutate: mockTaskMutate, isPending: false });
    mockUseCancelWorkflowRuntime.mockReturnValue({ mutate: mockRuntimeMutate, isPending: false });

    wrap(
      <SubmitSuccess
        taskId="runtime-correlation"
        workflowId="workflow-runtime-123"
        executionPath="workflow_runtime"
        onReset={onReset}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(mockRuntimeMutate).toHaveBeenCalledWith(
      "workflow-runtime-123",
      expect.objectContaining({ onSuccess: expect.any(Function) }),
    );
    expect(mockTaskMutate).not.toHaveBeenCalled();
    expect(onReset).toHaveBeenCalled();
  });

  it("stream error in success screen displays an error message (not silent)", async () => {
    let capturedOnError: ((msg: string) => void) | undefined;
    mockUseTaskStream.mockImplementation(
      (_id: string, _onChunk: unknown, onError?: (msg: string) => void) => {
        capturedOnError = onError;
      },
    );

    wrap(<SubmitSuccess taskId="task-err" onReset={vi.fn()} />);

    await act(async () => {
      capturedOnError?.("connection refused");
    });

    expect(screen.getByTestId("stream-error")).toHaveTextContent("connection refused");
  });
});
