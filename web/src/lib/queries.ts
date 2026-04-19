import { useQuery } from "@tanstack/react-query";
import { apiJson } from "./api";
import type { DashboardPayload, OverviewPayload, Task, TaskArtifact, TaskPrompt } from "@/types";

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

export function useTasks() {
  return useQuery<Task[], Error>({
    queryKey: ["tasks"],
    queryFn: ({ signal }) => apiJson<Task[]>("/tasks", { signal }),
  });
}

export function useTask(taskId: string | null) {
  return useQuery<Task, Error>({
    queryKey: ["task", taskId],
    queryFn: ({ signal }) => apiJson<Task>(`/tasks/${taskId}`, { signal }),
    enabled: !!taskId,
    staleTime: 0,
  });
}

export function useTaskArtifacts(taskId: string | null) {
  return useQuery<TaskArtifact[], Error>({
    queryKey: ["task-artifacts", taskId],
    queryFn: ({ signal }) => apiJson<TaskArtifact[]>(`/tasks/${taskId}/artifacts`, { signal }),
    enabled: !!taskId,
    staleTime: 0,
  });
}

export function useTaskPrompts(taskId: string | null) {
  return useQuery<TaskPrompt[], Error>({
    queryKey: ["task-prompts", taskId],
    queryFn: ({ signal }) => apiJson<TaskPrompt[]>(`/tasks/${taskId}/prompts`, { signal }),
    enabled: !!taskId,
    staleTime: 0,
  });
}
