import { HarnessRpcError } from "./errors";
import type {
  FetchLike,
  HarnessOptions,
  RpcEnvelope,
  RpcRequestEnvelope,
} from "./types";
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

export class RpcTransport {
  private readonly endpoint: string;
  private readonly fetchImpl: FetchLike;
  private readonly requestTimeoutMs: number;
  private readonly headers: Record<string, string>;
  private nextRequestId: number;

  constructor(options: HarnessOptions) {
    this.endpoint = `${normalizeBaseUrl(options.baseUrl)}/rpc`;
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
    this.nextRequestId = 1;
  }

  async request<T>(method: string, params: Record<string, unknown>): Promise<T> {
    const payload: RpcRequestEnvelope = {
      jsonrpc: "2.0",
      id: this.nextRequestId++,
      method,
      params,
    };

    const responseText = await withTimeout(
      this.fetchImpl(this.endpoint, {
        method: "POST",
        headers: this.headers,
        body: JSON.stringify(payload),
      }).then(async (response) => {
        const body = await response.text();
        if (!response.ok) {
          throw new Error(`HTTP ${response.status}: ${body}`);
        }
        return body;
      }),
      this.requestTimeoutMs,
      `RPC request timeout after ${this.requestTimeoutMs}ms for method '${method}'`,
    );

    let parsed: RpcEnvelope<T>;
    try {
      parsed = JSON.parse(responseText) as RpcEnvelope<T>;
    } catch (error) {
      throw new Error(
        `Failed to parse RPC response for '${method}': ${(error as Error).message}`,
      );
    }

    if (parsed.error) {
      throw new HarnessRpcError(parsed.error);
    }

    if (typeof parsed.result === "undefined") {
      throw new Error(`Missing RPC result for '${method}'`);
    }

    return parsed.result;
  }
}
