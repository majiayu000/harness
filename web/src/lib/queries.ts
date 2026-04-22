import { useQuery } from "@tanstack/react-query";
import { apiJson } from "./api";
import type { DashboardPayload, OperatorSnapshotPayload, OverviewPayload, Task, Worktree } from "@/types";

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

export function useWorktrees() {
  return useQuery<Worktree[], Error>({
    queryKey: ["worktrees"],
    queryFn: ({ signal }) => apiJson<Worktree[]>("/api/worktrees", { signal }),
    refetchInterval: 5000,
    refetchIntervalInBackground: true,
  });
}
