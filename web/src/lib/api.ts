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
    public readonly details?: unknown,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

function authHeaders(): Record<string, string> {
  const tok = (globalThis.sessionStorage?.getItem?.(TOKEN_KEY) ?? "").trim();
  return tok ? { Authorization: `Bearer ${tok}` } : {};
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
    let body: unknown = null;
    let message = `${path} → HTTP ${resp.status}`;
    try {
      body = await resp.clone().json();
      if (body && typeof body === "object") {
        const error = "error" in body ? body.error : undefined;
        const errors = "errors" in body ? body.errors : undefined;
        if (typeof error === "string" && error.trim()) {
          message = error;
        } else if (Array.isArray(errors) && typeof errors[0] === "string") {
          message = errors[0];
        }
      }
    } catch {
      // Non-JSON error bodies fall back to the HTTP status message above.
    }
    throw new ApiError(resp.status, message, body);
  }
  return resp;
}

export async function apiJson<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await apiFetch(path, init);
  return (await resp.json()) as T;
}
