import type { RpcErrorPayload } from "./types";

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
