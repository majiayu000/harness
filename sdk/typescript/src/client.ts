import { HarnessThread, parseTokenUsage, parseTurnItems } from "./thread";
import { HttpTransport } from "./transport";
import { HarnessHttpError } from "./errors";
import type {
  ErrorItem,
  HarnessOptions,
  StartThreadOptions,
  TurnItem,
  TurnSnapshot,
  TurnStatus,
} from "./types";

interface RuntimeSubmissionCreated {
  task_id: string;
  submission_id?: string;
  execution_path: string;
}

interface RuntimeSubmissionDetail {
  submission_id: string;
  status: string;
  project?: string | null;
  error?: string | null;
  created_at?: string | null;
  updated_at?: string | null;
  token_usage?: unknown;
  pending_approvals?: unknown;
}

interface RuntimeArtifact {
  artifact_type: string;
  content: string;
}

export class Harness {
  private readonly transport: HttpTransport;
  private readonly defaultCwd: string | undefined;
  private readonly defaultPollIntervalMs: number;
  private readonly defaultRunTimeoutMs: number;

  constructor(options: HarnessOptions = {}) {
    this.transport = new HttpTransport(options);
    this.defaultCwd = options.cwd;
    this.defaultPollIntervalMs = options.defaultPollIntervalMs ?? 500;
    this.defaultRunTimeoutMs = options.defaultRunTimeoutMs ?? 30_000;
  }

  async startThread(options: StartThreadOptions = {}): Promise<HarnessThread> {
    const cwd = options.cwd ?? this.defaultCwd;
    if (typeof cwd !== "string" || cwd.length === 0) {
      throw new Error("`cwd` is required; pass Harness({ cwd }) or startThread({ cwd }).");
    }
    return new HarnessThread(this, cwd);
  }

  async resumeThread(project: string): Promise<HarnessThread> {
    return this.thread(project);
  }

  thread(project: string): HarnessThread {
    if (typeof project !== "string" || project.length === 0) {
      throw new Error("project handle must not be empty");
    }
    return new HarnessThread(this, project);
  }

  async startTurn(project: string, prompt: string): Promise<{ turn_id: string }> {
    const created = await this.transport.request<RuntimeSubmissionCreated>(
      "POST",
      "/api/workflows/runtime/submissions",
      { project, prompt },
    );
    if (created.execution_path !== "workflow_runtime") {
      throw new Error("server did not create a workflow-runtime submission");
    }
    const turnId = created.submission_id ?? created.task_id;
    if (!turnId) {
      throw new Error("runtime submission response is missing submission_id");
    }
    return { turn_id: turnId };
  }

  async turnStatus(turnId: string, project?: string): Promise<TurnSnapshot> {
    const detail = await this.transport.request<RuntimeSubmissionDetail>(
      "GET",
      `/api/workflows/runtime/submissions/${encodeURIComponent(turnId)}`,
    );
    if (typeof detail !== "object" || detail === null || Array.isArray(detail)) {
      throw new Error("runtime submission detail must be an object");
    }
    const status = runtimeStatusToTurnStatus(detail.status);
    const items = await this.runtimeItems(
      turnId,
      status,
      detail.error,
      detail.pending_approvals,
    );
    const tokenUsage = parseTokenUsage(detail.token_usage);
    return {
      id: detail.submission_id || turnId,
      thread_id: detail.project || project || "",
      status,
      items,
      ...(tokenUsage ? { token_usage: tokenUsage } : {}),
      ...(detail.created_at ? { started_at: detail.created_at } : {}),
      ...(status !== "running" && detail.updated_at
        ? { completed_at: detail.updated_at }
        : {}),
    };
  }

  private async runtimeItems(
    turnId: string,
    status: TurnStatus,
    taskError: string | null | undefined,
    pendingApprovals: unknown,
  ): Promise<TurnItem[]> {
    if (status === "running") {
      return parsePendingApprovals(pendingApprovals);
    }
    let artifacts: unknown;
    try {
      artifacts = await this.transport.request<unknown>(
        "GET",
        `/api/workflows/runtime/submissions/${encodeURIComponent(turnId)}/artifacts`,
      );
    } catch (error) {
      if (!(error instanceof HarnessHttpError) || error.status !== 404) {
        throw error;
      }
      artifacts = [];
    }
    if (!Array.isArray(artifacts)) {
      throw new Error("runtime artifacts response must be an array");
    }
    const envelope = [...artifacts]
      .reverse()
      .find(
        (artifact): artifact is RuntimeArtifact =>
          typeof artifact === "object" &&
          artifact !== null &&
          !Array.isArray(artifact) &&
          "artifact_type" in artifact &&
          artifact.artifact_type === "activity_result_envelope" &&
          "content" in artifact &&
          typeof artifact.content === "string",
      );
    const finalResult = parseFinalResult(envelope?.content);
    const items: TurnItem[] = [];
    if (finalResult.summary) {
      items.push({ type: "agent_reasoning", content: finalResult.summary });
    }
    const error = finalResult.error ?? taskError;
    if (error) {
      const item: ErrorItem = { type: "error", code: 1, message: error };
      items.push(item);
    }
    return items;
  }

  resolvePollIntervalMs(value: number | undefined): number {
    return Math.max(10, value ?? this.defaultPollIntervalMs);
  }

  resolveRunTimeoutMs(value: number | undefined): number {
    return Math.max(1, value ?? this.defaultRunTimeoutMs);
  }
}

function parsePendingApprovals(value: unknown): TurnItem[] {
  if (typeof value === "undefined") {
    return [];
  }
  if (!Array.isArray(value)) {
    throw new Error("runtime pending_approvals must be an array");
  }
  const items = parseTurnItems(value);
  if (
    !items ||
    items.some(
      (item) =>
        item.type !== "approval_request" ||
        typeof item.id !== "string" ||
        item.id.length === 0 ||
        (typeof item.approved !== "undefined" && item.approved !== null),
    )
  ) {
    throw new Error("runtime pending_approvals contains an invalid approval request");
  }
  return items;
}

function runtimeStatusToTurnStatus(status: string): TurnStatus {
  if (status === "done") return "completed";
  if (status === "failed") return "failed";
  if (status === "cancelled") return "cancelled";
  return "running";
}

function parseFinalResult(content: string | undefined): {
  summary?: string;
  error?: string;
} {
  if (!content) return {};
  try {
    const envelope = JSON.parse(content) as unknown;
    if (typeof envelope !== "object" || envelope === null || Array.isArray(envelope)) {
      throw new Error("expected an object");
    }
    const finalResult =
      "final_result" in envelope &&
      typeof envelope.final_result === "object" &&
      envelope.final_result !== null &&
      !Array.isArray(envelope.final_result)
        ? (envelope.final_result as { summary?: unknown; error?: unknown })
        : {};
    return {
      ...(typeof finalResult.summary === "string"
        ? { summary: finalResult.summary }
        : {}),
      ...(typeof finalResult.error === "string"
        ? { error: finalResult.error }
        : {}),
    };
  } catch (error) {
    throw new Error(
      `Invalid activity_result_envelope artifact: ${(error as Error).message}`,
    );
  }
}
