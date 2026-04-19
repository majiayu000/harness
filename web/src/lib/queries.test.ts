import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { useTask, useTaskArtifacts, useTaskPrompts } from "./queries";

vi.mock("./api", () => ({
  apiJson: vi.fn(),
}));

import { apiJson } from "./api";

function wrapper(client: QueryClient) {
  return ({ children }: { children: React.ReactNode }) =>
    React.createElement(QueryClientProvider, { client }, children);
}

describe("useTask / useTaskArtifacts / useTaskPrompts", () => {
  let client: QueryClient;

  beforeEach(() => {
    client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  });

  afterEach(() => {
    vi.clearAllMocks();
    client.clear();
  });

  it("useTask skips fetch when taskId is null", () => {
    renderHook(() => useTask(null), { wrapper: wrapper(client) });
    expect(vi.mocked(apiJson)).not.toHaveBeenCalled();
  });

  it("useTaskArtifacts fetches /tasks/{id}/artifacts", async () => {
    vi.mocked(apiJson).mockResolvedValue([]);
    const { result } = renderHook(() => useTaskArtifacts("abc-123"), { wrapper: wrapper(client) });
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(vi.mocked(apiJson)).toHaveBeenCalledWith("/tasks/abc-123/artifacts", expect.anything());
  });

  it("useTaskPrompts fetches /tasks/{id}/prompts", async () => {
    vi.mocked(apiJson).mockResolvedValue([]);
    const { result } = renderHook(() => useTaskPrompts("abc-123"), { wrapper: wrapper(client) });
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(vi.mocked(apiJson)).toHaveBeenCalledWith("/tasks/abc-123/prompts", expect.anything());
  });

  it("useTaskArtifacts skips fetch when taskId is null", () => {
    renderHook(() => useTaskArtifacts(null), { wrapper: wrapper(client) });
    expect(vi.mocked(apiJson)).not.toHaveBeenCalled();
  });
});
