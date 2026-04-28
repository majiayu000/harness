import { useEffect, useRef } from "react";
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiFetch } from "./api";
import type { DashboardPayload, FullTask, OperatorSnapshotPayload, OverviewPayload, StreamItem, Task } from "@/types";

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
