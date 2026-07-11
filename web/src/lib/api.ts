/**
 * Key used to persist the bearer token across reloads (session-scoped).
 * Shared with the old dashboard.js so existing tabs don't lose auth.
 */
export const TOKEN_KEY = "harness_token";

/**
 * EventTarget that dispatches a single "unauthorized" event type when the
 * server returns 401. The app mounts a listener in TokenPrompt.
 */
export const unauthorizedEvents = new EventTarget();

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

function authHeaders(): Record<string, string> {
  const tok = (globalThis.sessionStorage?.getItem?.(TOKEN_KEY) ?? "").trim();
  return tok ? { Authorization: `Bearer ${tok}` } : {};
}

async function apiErrorMessage(resp: Response, path: string): Promise<string> {
  const fallback = `${path} → HTTP ${resp.status}`;
  if (!resp.headers.get("Content-Type")?.toLowerCase().includes("json")) {
    return fallback;
  }

  let payload: unknown;
  try {
    payload = await resp.json();
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    return `${fallback} (invalid JSON error response: ${detail})`;
  }
  if (typeof payload !== "object" || payload === null || !("error" in payload)) {
    return fallback;
  }
  const detail = (payload as { error?: unknown }).error;
  return typeof detail === "string" && detail.trim() ? detail.trim() : fallback;
}

export async function apiFetch(
  path: string,
  init: RequestInit = {},
): Promise<Response> {
  const merged: RequestInit = {
    ...init,
    headers: {
      Accept: "application/json",
      ...authHeaders(),
      ...(init.headers ?? {}),
    },
  };
  const resp = await fetch(path, merged);
  if (resp.status === 401) {
    unauthorizedEvents.dispatchEvent(new Event("unauthorized"));
    throw new ApiError(401, `${path} → 401`);
  }
  if (!resp.ok) {
    throw new ApiError(resp.status, await apiErrorMessage(resp, path));
  }
  return resp;
}

export async function apiJson<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await apiFetch(path, init);
  return (await resp.json()) as T;
}
