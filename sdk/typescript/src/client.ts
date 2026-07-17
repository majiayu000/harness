import { HarnessThread } from "./thread";
import { HttpTransport } from "./transport";
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
    const status = runtimeStatusToTurnStatus(detail.status);
    const items = await this.runtimeItems(turnId, status, detail.error);
    return {
      id: detail.submission_id || turnId,
      thread_id: detail.project || project || "",
      status,
      items,
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
  ): Promise<TurnItem[]> {
    if (status === "running") {
      return [];
    }
    const artifacts = await this.transport.request<RuntimeArtifact[]>(
      "GET",
      `/api/workflows/runtime/submissions/${encodeURIComponent(turnId)}/artifacts`,
    );
    const envelope = [...artifacts]
      .reverse()
      .find((artifact) => artifact.artifact_type === "activity_result_envelope");
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
    const envelope = JSON.parse(content) as {
      final_result?: { summary?: unknown; error?: unknown };
    };
    return {
      ...(typeof envelope.final_result?.summary === "string"
        ? { summary: envelope.final_result.summary }
        : {}),
      ...(typeof envelope.final_result?.error === "string"
        ? { error: envelope.final_result.error }
        : {}),
    };
  } catch (error) {
    throw new Error(
      `Invalid activity_result_envelope artifact: ${(error as Error).message}`,
    );
  }
}
