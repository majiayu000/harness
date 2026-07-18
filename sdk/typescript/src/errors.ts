import type { RpcErrorPayload } from "./types";

export class HarnessHttpError extends Error {
  readonly status: number;
  readonly data: unknown;

  constructor(status: number, message: string, data: unknown = undefined) {
    super(message);
    this.name = "HarnessHttpError";
    this.status = status;
    this.data = data;
  }
}

/** @deprecated JSON-RPC lifecycle methods were removed. Use HarnessHttpError. */
export class HarnessRpcError extends Error {
  readonly code: number;
  readonly data: unknown;

  constructor(payload: RpcErrorPayload) {
    super(payload.message);
    this.name = "HarnessRpcError";
    this.code = payload.code;
    this.data = payload.data;
  }
}
