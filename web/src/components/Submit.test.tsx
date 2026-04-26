import { describe, expect, it, vi, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { Submit } from "@/routes/dashboard/Submit";

vi.mock("@/lib/queries", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/queries")>();
  return {
    ...actual,
    useProjects: () => ({
      data: [
        { id: "proj-a", root: "/a" },
        { id: "proj-b", root: "/b" },
      ],
      isLoading: false,
    }),
  };
});

const originalFetch = global.fetch;
afterEach(() => {
  global.fetch = originalFetch;
  vi.restoreAllMocks();
});

function makeWrapper() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return function Wrapper({ children }: { children: React.ReactNode }) {
    return React.createElement(QueryClientProvider, { client: qc }, children);
  };
}

describe("<Submit>", () => {
  it("renders project select with options from useProjects", () => {
    render(<Submit />, { wrapper: makeWrapper() });
    expect(screen.getByRole("combobox")).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "proj-a" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "proj-b" })).toBeInTheDocument();
  });

  it("shows placeholder option when project list is available", () => {
    render(<Submit />, { wrapper: makeWrapper() });
    expect(screen.getByRole("option", { name: /select a project/i })).toBeInTheDocument();
  });

  it("clicking a template fills title and description", () => {
    render(<Submit />, { wrapper: makeWrapper() });
    fireEvent.click(screen.getByRole("button", { name: "Fix a bug" }));
    expect(screen.getByLabelText(/title/i)).toHaveValue("Fix a bug");
    expect((screen.getByLabelText(/description/i) as HTMLTextAreaElement).value).toMatch(
      /Describe the bug/i,
    );
  });

  it("calls onTaskCreated with task id on successful submission", async () => {
    const onTaskCreated = vi.fn();
    global.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ id: "task-xyz" }), { status: 200 }),
    ) as unknown as typeof fetch;

    render(<Submit onTaskCreated={onTaskCreated} />, { wrapper: makeWrapper() });
    fireEvent.change(screen.getByLabelText(/title/i), { target: { value: "My task" } });
    fireEvent.change(screen.getByLabelText(/description/i), {
      target: { value: "Some description" },
    });
    fireEvent.click(screen.getByRole("button", { name: /submit task/i }));
    await waitFor(() => expect(onTaskCreated).toHaveBeenCalledWith("task-xyz"));
  });

  it("shows error message on API failure", async () => {
    global.fetch = vi.fn().mockResolvedValue(
      new Response("server error", { status: 500 }),
    ) as unknown as typeof fetch;

    render(<Submit />, { wrapper: makeWrapper() });
    fireEvent.change(screen.getByLabelText(/title/i), { target: { value: "My task" } });
    fireEvent.change(screen.getByLabelText(/description/i), {
      target: { value: "Some description" },
    });
    fireEvent.click(screen.getByRole("button", { name: /submit task/i }));
    await waitFor(() => expect(screen.getByText(/HTTP 500/i)).toBeInTheDocument());
  });

  it("disables submit button while request is in-flight", async () => {
    let resolveFetch!: () => void;
    const fetchPromise = new Promise<Response>((resolve) => {
      resolveFetch = () =>
        resolve(new Response(JSON.stringify({ id: "t1" }), { status: 200 }));
    });
    global.fetch = vi.fn().mockReturnValue(fetchPromise) as unknown as typeof fetch;

    render(<Submit />, { wrapper: makeWrapper() });
    fireEvent.change(screen.getByLabelText(/title/i), { target: { value: "My task" } });
    fireEvent.change(screen.getByLabelText(/description/i), {
      target: { value: "Some description" },
    });
    fireEvent.click(screen.getByRole("button", { name: /submit task/i }));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /submitting/i })).toBeDisabled(),
    );
    resolveFetch();
  });
});
