import { describe, expect, it, vi, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import * as api from "./api";
import { useTasks } from "./queries";

function makeWrapper() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

describe("useTasks", () => {
  afterEach(() => vi.restoreAllMocks());

  it("fetches from /api/tasks", async () => {
    const spy = vi.spyOn(api, "apiJson").mockResolvedValue([]);
    const { result } = renderHook(() => useTasks(), { wrapper: makeWrapper() });
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(spy).toHaveBeenCalledWith(
      "/api/tasks",
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );
  });
});
