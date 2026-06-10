import { useEffect, useRef } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { apiJson, apiFetch } from "./api";
import type {
  DashboardPayload,
  EvalDashboardResponse,
  EvalQualitySnapshotResponse,
  EvalRunsResponse,
  FullTask,
  OperatorSnapshotPayload,
  OverviewPayload,
  StreamItem,
  TaskListResponse,
  UsageMonitorPayload,
  WorkflowRuntimeTreePayload,
} from "@/types";

export function useDashboard() {
  return useQuery<DashboardPayload, Error>({
    queryKey: ["dashboard"],
    queryFn: ({ signal }) => apiJson<DashboardPayload>("/api/dashboard", { signal }),
  });
}

export function useOverview() {
  return useQuery<OverviewPayload, Error>({
    queryKey: ["overview"],
    queryFn: ({ signal }) => apiJson<OverviewPayload>("/api/overview", { signal }),
  });
}

export function useOperatorSnapshot() {
  return useQuery<OperatorSnapshotPayload, Error>({
    queryKey: ["operator-snapshot"],
    queryFn: ({ signal }) => apiJson<OperatorSnapshotPayload>("/api/operator-snapshot", { signal }),
    refetchInterval: 30_000,
  });
}

export function useUsageMonitor() {
  return useQuery<UsageMonitorPayload, Error>({
    queryKey: ["usage-monitor"],
    queryFn: ({ signal }) => apiJson<UsageMonitorPayload>("/api/usage-monitor", { signal }),
    refetchInterval: 5_000,
  });
}

export function useEvalDashboard(limit = 50) {
  return useQuery<EvalDashboardResponse, Error>({
    queryKey: ["eval-dashboard", limit],
    queryFn: async ({ signal }) => {
      const runs = await apiJson<EvalRunsResponse>(`/api/evals/runs?limit=${limit}`, { signal });
      const rows = await Promise.all(
        runs.runs.map(async (run) => {
          if (!run.quality_snapshot_id) {
            return {
              run,
              quality_snapshot: null,
              quality_snapshot_error: null,
            };
          }
          try {
            const snapshot = await apiJson<EvalQualitySnapshotResponse>(
              `/api/evals/quality-snapshots/${encodeURIComponent(run.quality_snapshot_id)}`,
              { signal },
            );
            return {
              run,
              quality_snapshot: snapshot.quality_snapshot,
              quality_snapshot_error: null,
            };
          } catch (error) {
            return {
              run,
              quality_snapshot: null,
              quality_snapshot_error: error instanceof Error ? error.message : "snapshot unavailable",
            };
          }
        }),
      );
      return { rows };
    },
    refetchInterval: 30_000,
  });
}

export interface TaskListParams {
  status?: string;
  scheduler_state?: string;
  active?: boolean;
  kind?: string;
  source?: string;
  repo?: string;
  project_id?: string;
  limit?: number;
  cursor?: string;
}

function taskListPath(params: TaskListParams): string {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === "") continue;
    query.set(key, String(value));
  }
  const suffix = query.toString();
  return suffix ? `/tasks?${suffix}` : "/tasks";
}

export function useTasks(params: TaskListParams = { active: true, limit: 200 }) {
  return useQuery<TaskListResponse, Error>({
    queryKey: ["tasks", params],
    // Task list/detail/stream routes are exposed at `/tasks`; only aggregate
    // dashboard endpoints are mounted under `/api/*`.
    queryFn: ({ signal }) => apiJson<TaskListResponse>(taskListPath(params), { signal }),
  });
}

async function fetchAllTaskPages(params: TaskListParams, signal?: AbortSignal): Promise<TaskListResponse> {
  const seenCursors = new Set<string>();
  const rows: TaskListResponse["data"] = [];
  let cursor = params.cursor;
  let latest: TaskListResponse | null = null;

  for (;;) {
    latest = await apiJson<TaskListResponse>(taskListPath({ ...params, cursor }), { signal });
    rows.push(...latest.data);
    const nextCursor = latest.page.next_cursor ?? null;
    if (!latest.page.has_more || !nextCursor) break;
    if (seenCursors.has(nextCursor)) {
      throw new Error("Task pagination returned a repeated cursor");
    }
    seenCursors.add(nextCursor);
    cursor = nextCursor;
  }

  if (!latest) {
    throw new Error("Task pagination returned no response");
  }

  return {
    ...latest,
    data: rows,
    page: { ...latest.page, has_more: false, next_cursor: null },
  };
}

export function useAllTasks(params: TaskListParams = { limit: 200 }) {
  return useQuery<TaskListResponse, Error>({
    queryKey: ["tasks", "all-pages", params],
    queryFn: ({ signal }) => fetchAllTaskPages(params, signal),
  });
}

export function useWorkflowRuntimeTree(projectId?: string | null) {
  return useQuery<WorkflowRuntimeTreePayload, Error>({
    queryKey: ["workflow-runtime-tree", projectId ?? null],
    queryFn: ({ signal }) => {
      const params = new URLSearchParams();
      params.set("limit", "100");
      if (projectId) params.set("project_id", projectId);
      return apiJson<WorkflowRuntimeTreePayload>(`/api/workflows/runtime/tree?${params}`, {
        signal,
      });
    },
    retry: false,
    refetchInterval: 30_000,
  });
}

export function useTaskDetail(id: string | null) {
  return useQuery<FullTask, Error>({
    queryKey: ["task", id],
    queryFn: ({ signal }) => apiJson<FullTask>(`/tasks/${id}`, { signal }),
    enabled: !!id,
  });
}

export function useTaskStream(
  id: string | null,
  onChunk: (text: string) => void,
  onError?: (message: string) => void,
): void {
  const onChunkRef = useRef(onChunk);
  const onErrorRef = useRef(onError);
  useEffect(() => {
    onChunkRef.current = onChunk;
    onErrorRef.current = onError;
  });

  useEffect(() => {
    if (!id) return;
    const controller = new AbortController();

    (async () => {
      try {
        const resp = await apiFetch(`/tasks/${id}/stream`, {
          signal: controller.signal,
          headers: { Accept: "text/event-stream" },
        });
        if (!resp.body) return;

        const reader = resp.body.getReader();
        const decoder = new TextDecoder();
        let buf = "";

        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          buf += decoder.decode(value, { stream: true });
          const parts = buf.split("\n\n");
          buf = parts.pop() ?? "";
          for (const part of parts) {
            const dataLine = part.split("\n").find((l) => l.startsWith("data: "));
            if (!dataLine) continue;
            try {
              const item = JSON.parse(dataLine.slice(6)) as StreamItem;
              if (item.type === "MessageDelta") {
                onChunkRef.current(item.text);
              } else if (item.type === "Error") {
                onErrorRef.current?.(item.message);
                reader.cancel();
                return;
              } else if (item.type === "Done") {
                reader.cancel();
                return;
              }
            } catch {
              // malformed SSE line — skip
            }
          }
        }
      } catch (err) {
        if (err instanceof Error && err.name !== "AbortError") {
          onErrorRef.current?.(err.message);
        }
        // AbortError — clean exit
      }
    })();

    return () => controller.abort();
  }, [id]);
}

export function useCancelTask() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => apiFetch(`/tasks/${id}/cancel`, { method: "POST" }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["tasks"] }),
  });
}

export function useCancelWorkflowRuntime() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (workflowId: string) =>
      apiFetch("/api/workflows/runtime/cancel", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ workflow_id: workflowId }),
      }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["tasks"] });
      await queryClient.invalidateQueries({ queryKey: ["workflow-runtime-tree"] });
    },
  });
}

export interface WorktreeCard {
  taskId: string;
  workspacePath: string;
  pathShort: string;
  sourceRepo: string;
  repo: string | null;
  runtimeWorkflowId: string | null;
  branch: string;
  status: string;
  phase: string;
  description: string | null;
  turn: number;
  maxTurns: number | null;
  createdAt: string;
  durationSecs: number;
  prUrl: string | null;
  project: string | null;
}

interface WorktreeApiEntry {
  task_id: string;
  branch: string;
  workspace_path: string;
  path_short: string;
  source_repo: string;
  repo: string | null;
  runtime_workflow_id: string | null;
  status: string;
  phase: string;
  description: string | null;
  turn: number;
  max_turns: number | null;
  created_at: string;
  duration_secs: number;
  pr_url: string | null;
  project: string | null;
}

function worktreeStatusRank(status: string): number {
  switch (status) {
    case "failed":
      return 0;
    case "agent_review":
    case "reviewing":
    case "review_generating":
    case "review_waiting":
      return 1;
    case "implementing":
    case "triaging":
    case "planning":
    case "planner_generating":
    case "planner_waiting":
      return 2;
    default:
      return 3;
  }
}

function normalizeMaxTurns(maxTurns: number | null): number | null {
  return typeof maxTurns === "number" && maxTurns > 0 ? maxTurns : null;
}

export async function registerProject(req: {
  id: string;
  root: string;
  default_agent?: string;
  max_concurrent?: number;
}): Promise<void> {
  await apiFetch("/projects", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
}

export function useWorktrees(): { cards: WorktreeCard[]; isLoading: boolean; error: Error | null } {
  const query = useQuery<WorktreeApiEntry[], Error>({
    queryKey: ["worktrees"],
    queryFn: ({ signal }) => apiJson<WorktreeApiEntry[]>("/api/worktrees", { signal }),
    refetchInterval: 5_000,
  });

  const cards = (query.data ?? [])
    .map((entry) => ({
      taskId: entry.task_id,
      workspacePath: entry.workspace_path,
      pathShort: entry.path_short,
      sourceRepo: entry.source_repo,
      repo: entry.repo,
      runtimeWorkflowId: entry.runtime_workflow_id,
      branch: entry.branch,
      status: entry.status,
      phase: entry.phase,
      description: entry.description,
      turn: entry.turn,
      maxTurns: normalizeMaxTurns(entry.max_turns),
      createdAt: entry.created_at,
      durationSecs: entry.duration_secs,
      prUrl: entry.pr_url,
      project: entry.project,
    }))
    .sort((left, right) => {
      const rankDiff = worktreeStatusRank(left.status) - worktreeStatusRank(right.status);
      if (rankDiff !== 0) return rankDiff;
      return right.durationSecs - left.durationSecs;
    });

  return { cards, isLoading: query.isLoading, error: query.error ?? null };
}
