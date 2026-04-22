import { useQuery } from "@tanstack/react-query";
import { apiJson } from "./api";
<<<<<<< HEAD
import type { DashboardPayload, OperatorSnapshotPayload, OverviewPayload, Task } from "@/types";
=======
import type { DashboardPayload, OverviewPayload, Task, TaskDetail, TaskPromptRecord } from "@/types";
>>>>>>> 996596d (Expose persisted task prompts from dashboard cards)

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

function hasTaskId(id: string | null): id is string {
  return Boolean(id?.trim());
}

export function useTaskDetail(id: string | null) {
  return useQuery<TaskDetail, Error>({
    queryKey: ["tasks", id, "detail"],
    enabled: hasTaskId(id),
    queryFn: ({ signal }) => apiJson<TaskDetail>(`/tasks/${id}`, { signal }),
  });
}

export function useTaskPrompts(id: string | null) {
  return useQuery<TaskPromptRecord[], Error>({
    queryKey: ["tasks", id, "prompts"],
    enabled: hasTaskId(id),
    queryFn: ({ signal }) => apiJson<TaskPromptRecord[]>(`/tasks/${id}/prompts`, { signal }),
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
