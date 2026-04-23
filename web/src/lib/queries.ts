import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { apiJson } from "./api";
import type {
  CreateTaskResponse,
  DashboardPayload,
  OperatorSnapshotPayload,
  OverviewPayload,
  ProjectValidationResult,
  RegisterProjectResponse,
  RegisteredProject,
  Task,
  TaskArtifactRecord,
  TaskDetail,
  TaskPromptRecord,
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

export function useTasks() {
  return useQuery<Task[], Error>({
    queryKey: ["tasks"],
    queryFn: ({ signal }) => apiJson<Task[]>("/tasks", { signal }),
  });
}

export function useProjects() {
  return useQuery<RegisteredProject[], Error>({
    queryKey: ["projects"],
    queryFn: ({ signal }) => apiJson<RegisteredProject[]>("/projects", { signal }),
  });
}

export function useTaskDetail(taskId: string | null) {
  return useQuery<TaskDetail, Error>({
    queryKey: ["task-detail", taskId],
    queryFn: ({ signal }) => apiJson<TaskDetail>(`/tasks/${taskId}`, { signal }),
    enabled: Boolean(taskId),
  });
}

export function useTaskArtifacts(taskId: string | null) {
  return useQuery<TaskArtifactRecord[], Error>({
    queryKey: ["task-artifacts", taskId],
    queryFn: ({ signal }) => apiJson<TaskArtifactRecord[]>(`/tasks/${taskId}/artifacts`, { signal }),
    enabled: Boolean(taskId),
  });
}

export function useTaskPrompts(taskId: string | null) {
  return useQuery<TaskPromptRecord[], Error>({
    queryKey: ["task-prompts", taskId],
    queryFn: ({ signal }) => apiJson<TaskPromptRecord[]>(`/tasks/${taskId}/prompts`, { signal }),
    enabled: Boolean(taskId),
  });
}

export function useValidateProject() {
  return useMutation<ProjectValidationResult, Error, { root: string }>({
    mutationFn: async ({ root }) =>
      apiJson<ProjectValidationResult>("/projects/validate", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ root }),
      }),
  });
}

export function useRegisterProject() {
  const queryClient = useQueryClient();
  return useMutation<RegisterProjectResponse, Error, { root: string }>({
    mutationFn: async ({ root }) =>
      apiJson<RegisterProjectResponse>("/projects", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ root }),
      }),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["projects"] }),
        queryClient.invalidateQueries({ queryKey: ["dashboard"] }),
        queryClient.invalidateQueries({ queryKey: ["overview"] }),
      ]);
    },
  });
}

export function useCreateTask() {
  const queryClient = useQueryClient();
  return useMutation<CreateTaskResponse, Error, { prompt: string; project?: string }>({
    mutationFn: async ({ prompt, project }) =>
      apiJson<CreateTaskResponse>("/tasks", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ prompt, project }),
      }),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["tasks"] }),
        queryClient.invalidateQueries({ queryKey: ["dashboard"] }),
      ]);
    },
  });
}

export interface WorktreeCard {
  taskId: string;
  pathShort: string;
  branch: string;
  status: string;
  turn: number;
  maxTurns: number | null;
  cpuPct: number | null;
  ramPct: number | null;
  diskBytes: number | null;
}

export function useWorktrees(): { cards: WorktreeCard[]; isLoading: boolean; error: Error | null } {
  const tasks = useTasks();
  const overview = useOverview();

  const isLoading = tasks.isLoading || overview.isLoading;
  const error = tasks.error ?? overview.error ?? null;

  const TERMINAL_STATUSES = new Set(["done", "failed", "cancelled"]);
  const runningTasks = (tasks.data ?? []).filter((t) => !TERMINAL_STATUSES.has(t.status));

  const cards: WorktreeCard[] = runningTasks.map((task) => {
    const project = (overview.data?.projects ?? []).find(
      (p) => p.id === task.project || p.root === task.project,
    );

    let pathShort = "—";
    if (project?.root) {
      const parts = project.root.replace(/\\/g, "/").split("/").filter(Boolean);
      pathShort = parts.slice(-2).join("/") || project.root;
    }

    const agentId = project?.agents?.[0];
    const runtime = agentId
      ? (overview.data?.runtimes ?? []).find((r) => r.id === agentId)
      : undefined;

    return {
      taskId: task.id,
      pathShort,
      branch: "—",
      status: task.status,
      turn: task.turn,
      maxTurns: null,
      cpuPct: runtime?.cpu_pct ?? null,
      ramPct: runtime?.ram_pct ?? null,
      diskBytes: null,
    };
  });

  return { cards, isLoading, error };
}
