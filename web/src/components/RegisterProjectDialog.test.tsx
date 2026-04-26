import { describe, expect, it, vi, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { RegisterProjectDialog } from "./RegisterProjectDialog";

const originalFetch = global.fetch;
afterEach(() => {
  global.fetch = originalFetch;
  vi.restoreAllMocks();
  sessionStorage.clear();
});

function makeWrapper() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return function Wrapper({ children }: { children: React.ReactNode }) {
    return React.createElement(QueryClientProvider, { client: qc }, children);
  };
}

describe("<RegisterProjectDialog>", () => {
  it("renders root path input, id input, and register button", () => {
    render(<RegisterProjectDialog open onClose={vi.fn()} />, { wrapper: makeWrapper() });
    expect(screen.getByPlaceholderText("/path/to/your/project")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("my-project")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /register$/i })).toBeInTheDocument();
  });

  it("submit button is disabled when root path is empty", () => {
    render(<RegisterProjectDialog open onClose={vi.fn()} />, { wrapper: makeWrapper() });
    expect(screen.getByRole("button", { name: /register$/i })).toBeDisabled();
  });

  it("shows server error inline when registration fails", async () => {
    global.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ error: "invalid root path: not a git repository" }), { status: 400 }),
    ) as unknown as typeof fetch;

    render(<RegisterProjectDialog open onClose={vi.fn()} />, { wrapper: makeWrapper() });
    fireEvent.change(screen.getByPlaceholderText("/path/to/your/project"), {
      target: { value: "/not/a/git/repo" },
    });
    fireEvent.click(screen.getByRole("button", { name: /register$/i }));
    await waitFor(() =>
      expect(screen.getByRole("alert")).toHaveTextContent(/invalid root path/i),
    );
  });

  it("calls onSuccess with id and closes on successful registration", async () => {
    const onSuccess = vi.fn();
    const onClose = vi.fn();
    global.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ id: "my-repo", root: "/my/repo" }), { status: 201 }),
    ) as unknown as typeof fetch;

    render(<RegisterProjectDialog open onClose={onClose} onSuccess={onSuccess} />, {
      wrapper: makeWrapper(),
    });
    fireEvent.change(screen.getByPlaceholderText("/path/to/your/project"), {
      target: { value: "/my/repo" },
    });
    fireEvent.click(screen.getByRole("button", { name: /register$/i }));
    await waitFor(() => {
      expect(onSuccess).toHaveBeenCalledWith("my-repo");
      expect(onClose).toHaveBeenCalled();
    });
  });

  it("fires onClose when Cancel button is clicked", () => {
    const onClose = vi.fn();
    render(<RegisterProjectDialog open onClose={onClose} />, { wrapper: makeWrapper() });
    fireEvent.click(screen.getByRole("button", { name: /cancel/i }));
    expect(onClose).toHaveBeenCalled();
  });
});
