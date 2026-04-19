import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { TaskSlideover } from "./TaskSlideover";

vi.mock("@/lib/useTaskStream", () => ({
  useTaskStream: vi.fn(() => ({ lines: [], connected: false, error: null })),
}));

vi.mock("@/lib/queries", () => ({
  useTask: vi.fn(() => ({ data: null })),
  useTaskArtifacts: vi.fn(() => ({ data: [] })),
  useTaskPrompts: vi.fn(() => ({ data: [] })),
}));

import { useTaskStream } from "@/lib/useTaskStream";
import { useTask, useTaskArtifacts, useTaskPrompts } from "@/lib/queries";

function wrap(ui: React.ReactElement) {
  const client = new QueryClient();
  return render(<QueryClientProvider client={client}>{ui}</QueryClientProvider>);
}

describe("<TaskSlideover>", () => {
  beforeEach(() => {
    vi.mocked(useTaskStream).mockReturnValue({ lines: [], connected: false, error: null });
    vi.mocked(useTask).mockReturnValue({ data: null } as unknown as ReturnType<typeof useTask>);
    vi.mocked(useTaskArtifacts).mockReturnValue({ data: [] } as unknown as ReturnType<typeof useTaskArtifacts>);
    vi.mocked(useTaskPrompts).mockReturnValue({ data: [] } as unknown as ReturnType<typeof useTaskPrompts>);
  });

  it("renders nothing when taskId is null", () => {
    const { container } = wrap(<TaskSlideover taskId={null} onClose={() => {}} />);
    expect(container.firstChild).toBeNull();
  });

  it("renders panel with all 4 tabs when taskId is set", () => {
    wrap(<TaskSlideover taskId="abc12345" onClose={() => {}} />);
    expect(screen.getByText("Stream")).toBeInTheDocument();
    expect(screen.getByText("Diff")).toBeInTheDocument();
    expect(screen.getByText("Review")).toBeInTheDocument();
    expect(screen.getByText("Events")).toBeInTheDocument();
  });

  it("shows truncated taskId in header", () => {
    wrap(<TaskSlideover taskId="abc12345def" onClose={() => {}} />);
    expect(screen.getByText("abc12345")).toBeInTheDocument();
  });

  it("close button calls onClose", () => {
    const onClose = vi.fn();
    wrap(<TaskSlideover taskId="abc12345" onClose={onClose} />);
    fireEvent.click(screen.getByLabelText("Close"));
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("Escape keydown calls onClose", () => {
    const onClose = vi.fn();
    wrap(<TaskSlideover taskId="abc12345" onClose={onClose} />);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("backdrop click calls onClose", () => {
    const onClose = vi.fn();
    wrap(<TaskSlideover taskId="abc12345" onClose={onClose} />);
    fireEvent.click(screen.getByRole("presentation", { hidden: true }));
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("Stream tab shows connecting placeholder before first SSE line", () => {
    wrap(<TaskSlideover taskId="abc12345" onClose={() => {}} />);
    expect(screen.getByText("connecting…")).toBeInTheDocument();
  });

  it("Stream tab shows lines when connected", () => {
    vi.mocked(useTaskStream).mockReturnValue({ lines: ["line one", "line two"], connected: true, error: null });
    wrap(<TaskSlideover taskId="abc12345" onClose={() => {}} />);
    expect(screen.getByText(/line one/)).toBeInTheDocument();
  });

  it("Diff tab shows — when artifacts is empty", () => {
    wrap(<TaskSlideover taskId="abc12345" onClose={() => {}} />);
    fireEvent.click(screen.getByText("Diff"));
    expect(screen.getByText("—")).toBeInTheDocument();
  });

  it("Diff tab colours + lines with text-ok class", () => {
    vi.mocked(useTaskArtifacts).mockReturnValue({
      data: [{ task_id: "abc12345", turn: 1, artifact_type: "diff", content: "+added line\n-removed line\n@@ context", created_at: "" }],
    } as unknown as ReturnType<typeof useTaskArtifacts>);
    wrap(<TaskSlideover taskId="abc12345" onClose={() => {}} />);
    fireEvent.click(screen.getByText("Diff"));
    expect(screen.getByText("+added line").className).toContain("text-ok");
    expect(screen.getByText("-removed line").className).toContain("text-danger");
    expect(screen.getByText("@@ context").className).toContain("text-ink-3");
  });

  it("Review tab shows preformatted prompt text", () => {
    vi.mocked(useTaskPrompts).mockReturnValue({
      data: [{ task_id: "abc12345", turn: 1, phase: "review", prompt: "review content here", created_at: "" }],
    } as ReturnType<typeof useTaskPrompts>);
    wrap(<TaskSlideover taskId="abc12345" onClose={() => {}} />);
    fireEvent.click(screen.getByText("Review"));
    expect(screen.getByText("review content here")).toBeInTheDocument();
  });

  it("Events tab shows task status and turn", () => {
    vi.mocked(useTask).mockReturnValue({
      data: { id: "abc12345", status: "implementing", turn: 3, pr_url: null, error: null, source: null, parent_id: null, repo: null, description: null, created_at: null, phase: null, depends_on: [], subtask_ids: [], project: null },
    } as unknown as ReturnType<typeof useTask>);
    wrap(<TaskSlideover taskId="abc12345" onClose={() => {}} />);
    fireEvent.click(screen.getByText("Events"));
    expect(screen.getByText("implementing")).toBeInTheDocument();
    expect(screen.getByText("3")).toBeInTheDocument();
  });

  it("tab click switches active tab without remount", () => {
    wrap(<TaskSlideover taskId="abc12345" onClose={() => {}} />);
    const diffTab = screen.getByText("Diff");
    fireEvent.click(diffTab);
    expect(diffTab).toHaveAttribute("aria-selected", "true");
    expect(screen.getByText("Stream")).toHaveAttribute("aria-selected", "false");
  });
});
