import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { apiFetch, runtimeSubmissionPath, TOKEN_KEY, unauthorizedEvents } from "./api";

describe("runtimeSubmissionPath", () => {
  it("encodes opaque submission handles as one path segment", () => {
    expect(runtimeSubmissionPath("github-pr-feedback::/repo::pr:42", "stream")).toBe(
      "/api/workflows/runtime/submissions/github-pr-feedback%3A%3A%2Frepo%3A%3Apr%3A42/stream",
    );
  });
});

describe("apiFetch", () => {
  const originalFetch = global.fetch;

  beforeEach(() => {
    sessionStorage.clear();
  });

  afterEach(() => {
    global.fetch = originalFetch;
    vi.restoreAllMocks();
  });

  it("injects Authorization header when token is set", async () => {
    sessionStorage.setItem(TOKEN_KEY, "abc123");
    const mock = vi.fn().mockResolvedValue(new Response("{}", { status: 200 }));
    global.fetch = mock as unknown as typeof fetch;

    await apiFetch("/api/overview");

    expect(mock).toHaveBeenCalledOnce();
    const [, init] = mock.mock.calls[0];
    const headers = new Headers((init as RequestInit).headers);
    expect(headers.get("Authorization")).toBe("Bearer abc123");
  });

  it("omits Authorization when no token", async () => {
    const mock = vi.fn().mockResolvedValue(new Response("{}", { status: 200 }));
    global.fetch = mock as unknown as typeof fetch;

    await apiFetch("/api/overview");

    const [, init] = mock.mock.calls[0];
    const headers = new Headers((init as RequestInit).headers);
    expect(headers.get("Authorization")).toBeNull();
  });

  it("dispatches unauthorized event on 401 and throws", async () => {
    global.fetch = vi
      .fn()
      .mockResolvedValue(new Response("{}", { status: 401 })) as unknown as typeof fetch;

    const handler = vi.fn();
    unauthorizedEvents.addEventListener("unauthorized", handler);

    await expect(apiFetch("/api/overview")).rejects.toThrow(/401/);
    expect(handler).toHaveBeenCalledOnce();

    unauthorizedEvents.removeEventListener("unauthorized", handler);
  });

  it("surfaces a structured API error message", async () => {
    global.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ error: "workflow not in blocked state" }), {
        status: 409,
        headers: { "Content-Type": "application/json" },
      }),
    ) as unknown as typeof fetch;

    await expect(apiFetch("/api/workflows/runtime/unblock")).rejects.toMatchObject({
      status: 409,
      message: "workflow not in blocked state",
    });
  });
});
