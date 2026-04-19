import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useTaskStream } from "./useTaskStream";
import { unauthorizedEvents } from "./api";

type FetchEventSourceCallback = {
  onopen?: (resp: Response) => Promise<void>;
  onmessage?: (ev: { data: string; event: string; id: string; retry?: number }) => void;
  onerror?: (err: unknown) => void;
  onclose?: () => void;
};

let capturedCallbacks: FetchEventSourceCallback = {};
let capturedAbortSignal: AbortSignal | null = null;

vi.mock("@microsoft/fetch-event-source", () => ({
  fetchEventSource: vi.fn((_url: string, opts: FetchEventSourceCallback & { signal?: AbortSignal }) => {
    capturedCallbacks = opts;
    capturedAbortSignal = opts.signal ?? null;
    return Promise.resolve();
  }),
}));

describe("useTaskStream", () => {
  beforeEach(() => {
    capturedCallbacks = {};
    capturedAbortSignal = null;
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it("initial state: connected false, lines empty, error null", () => {
    const { result } = renderHook(() => useTaskStream("task-1"));
    expect(result.current.connected).toBe(false);
    expect(result.current.lines).toEqual([]);
    expect(result.current.error).toBeNull();
  });

  it("resets state when taskId is null", () => {
    const { result } = renderHook(() => useTaskStream(null));
    expect(result.current.connected).toBe(false);
    expect(result.current.lines).toEqual([]);
    expect(result.current.error).toBeNull();
  });

  it("onopen fires → connected becomes true", async () => {
    const { result } = renderHook(() => useTaskStream("task-1"));
    await act(async () => {
      await capturedCallbacks.onopen?.(new Response(null, { status: 200 }));
    });
    expect(result.current.connected).toBe(true);
  });

  it("onmessage accumulates lines in order", async () => {
    const { result } = renderHook(() => useTaskStream("task-1"));
    await act(async () => {
      await capturedCallbacks.onopen?.(new Response(null, { status: 200 }));
      capturedCallbacks.onmessage?.({ data: "first", event: "", id: "" });
      capturedCallbacks.onmessage?.({ data: "second", event: "", id: "" });
    });
    expect(result.current.lines).toEqual(["first", "second"]);
  });

  it("changing taskId resets lines to empty", async () => {
    const { result, rerender } = renderHook(({ id }: { id: string }) => useTaskStream(id), {
      initialProps: { id: "task-1" },
    });
    await act(async () => {
      await capturedCallbacks.onopen?.(new Response(null, { status: 200 }));
      capturedCallbacks.onmessage?.({ data: "old line", event: "", id: "" });
    });
    expect(result.current.lines).toHaveLength(1);

    rerender({ id: "task-2" });
    expect(result.current.lines).toEqual([]);
  });

  it("unmount aborts the controller", () => {
    const { unmount } = renderHook(() => useTaskStream("task-1"));
    unmount();
    expect(capturedAbortSignal?.aborted).toBe(true);
  });

  it("ring buffer: >2000 events drops oldest, keeps newest 2000", async () => {
    const { result } = renderHook(() => useTaskStream("task-1"));
    await act(async () => {
      await capturedCallbacks.onopen?.(new Response(null, { status: 200 }));
      for (let i = 0; i < 2001; i++) {
        capturedCallbacks.onmessage?.({ data: `line-${i}`, event: "", id: "" });
      }
    });
    expect(result.current.lines).toHaveLength(2000);
    expect(result.current.lines[0]).toBe("line-1");
    expect(result.current.lines[1999]).toBe("line-2000");
  });

  it("401 response dispatches unauthorizedEvents and stops", async () => {
    const handler = vi.fn();
    unauthorizedEvents.addEventListener("unauthorized", handler);

    renderHook(() => useTaskStream("task-1"));
    await act(async () => {
      await capturedCallbacks.onopen?.(new Response(null, { status: 401 }));
    });

    expect(handler).toHaveBeenCalledOnce();
    expect(capturedAbortSignal?.aborted).toBe(true);
    unauthorizedEvents.removeEventListener("unauthorized", handler);
  });
});
