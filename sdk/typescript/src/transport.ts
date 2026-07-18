import { HarnessHttpError } from "./errors";
import type { FetchLike, HarnessOptions } from "./types";
import { normalizeBaseUrl, withTimeout } from "./utils";

function resolveFetch(customFetch: FetchLike | undefined): FetchLike {
  if (customFetch) {
    return customFetch;
  }

  const globalFetch = (globalThis as { fetch?: unknown }).fetch;
  if (typeof globalFetch !== "function") {
    throw new Error("No fetch implementation available. Pass `fetch` in HarnessOptions.");
  }

  return (url, init) => (globalFetch as FetchLike)(url, init);
}

export class HttpTransport {
  private readonly baseUrl: string;
  private readonly fetchImpl: FetchLike;
  private readonly requestTimeoutMs: number;
  private readonly headers: Record<string, string>;

  constructor(options: HarnessOptions) {
    this.baseUrl = normalizeBaseUrl(options.baseUrl);
    this.fetchImpl = resolveFetch(options.fetch);
    this.requestTimeoutMs = options.requestTimeoutMs ?? 15_000;
    const headers: Record<string, string> = {
      "content-type": "application/json",
      ...(options.headers ?? {}),
    };
    const token = options.apiToken?.trim();
    if (token) {
      headers.authorization = `Bearer ${token}`;
    }
    this.headers = headers;
  }

  async request<T>(
    method: "GET" | "POST",
    path: string,
    body?: Record<string, unknown>,
  ): Promise<T> {
    const responseText = await withTimeout(
      this.fetchImpl(`${this.baseUrl}${path}`, {
        method,
        headers: this.headers,
        ...(body ? { body: JSON.stringify(body) } : {}),
      }).then(async (response) => {
        const responseBody = await response.text();
        if (!response.ok) {
          let data: unknown = responseBody;
          try {
            data = JSON.parse(responseBody) as unknown;
          } catch {
            // Preserve non-JSON error bodies verbatim.
          }
          const message =
            typeof data === "object" && data !== null && "error" in data
              ? String((data as { error: unknown }).error)
              : `HTTP ${response.status}`;
          throw new HarnessHttpError(response.status, message, data);
        }
        return responseBody;
      }),
      this.requestTimeoutMs,
      `HTTP request timeout after ${this.requestTimeoutMs}ms for ${method} ${path}`,
    );

    try {
      return JSON.parse(responseText) as T;
    } catch (error) {
      throw new Error(
        `Failed to parse HTTP response for ${method} ${path}: ${(error as Error).message}`,
      );
    }
  }
}
