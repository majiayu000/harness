import { useState } from "react";
import { useTasks } from "@/lib/queries";
import { TaskDetailPanel } from "./TaskDetailPanel";

interface Props {
  projectFilter?: string | null;
  selectedTaskId: string | null;
  onSelectTask: (taskId: string) => void;
}

const TERMINAL_STATUSES = new Set(["done", "failed", "cancelled"]);

export function History({ projectFilter, selectedTaskId, onSelectTask }: Props) {
  const { data, isLoading, isError } = useTasks();
  const [filter, setFilter] = useState<"all" | "done" | "failed">("all");
  const [query, setQuery] = useState("");
  const normalizedQuery = query.trim().toLowerCase();
  const rows = (data ?? [])
    .filter((task) => TERMINAL_STATUSES.has(task.status))
    .filter((task) => !projectFilter || task.project === projectFilter)
    .filter((task) => filter === "all" || task.status === filter)
    .filter((task) => {
      if (!normalizedQuery) return true;
      const haystack = `${task.description ?? ""} ${task.repo ?? ""} ${task.id}`.toLowerCase();
      return haystack.includes(normalizedQuery);
    });

  return (
    <div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_380px]">
      <div>
        <div className="flex gap-2 mb-4">
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search history…"
            className="flex-1 h-[30px] bg-bg-1 border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
          />
          <select
            value={filter}
            onChange={(e) => setFilter(e.target.value as "all" | "done" | "failed")}
            className="h-[30px] bg-bg-1 border border-line-2 text-ink font-mono text-[12px] px-2 rounded-[3px]"
          >
            <option value="all">All</option>
            <option value="done">Done</option>
            <option value="failed">Failed</option>
          </select>
        </div>
        <div className="border border-line bg-bg-1">
          {isLoading ? (
            <div className="p-4 font-mono text-[12px] text-ink-3">Loading recent tasks…</div>
          ) : isError ? (
            <div className="p-4 font-mono text-[12px] text-rust">Failed to load recent tasks.</div>
          ) : rows.length === 0 ? (
            <div className="p-4 font-mono text-[12px] text-ink-3">No completed tasks yet.</div>
          ) : (
            <div className="divide-y divide-line">
              {rows.map((task) => (
                <button
                  key={task.id}
                  type="button"
                  onClick={() => onSelectTask(task.id)}
                  className={`w-full px-4 py-3 text-left transition-colors ${
                    selectedTaskId === task.id ? "bg-rust/10" : "hover:bg-bg"
                  }`}
                >
                  <div className="flex items-center justify-between gap-3">
                    <div>
                      <div className="text-[13px] text-ink">
                        {task.description?.trim() || task.repo || task.id}
                      </div>
                      <div className="mt-1 font-mono text-[11px] text-ink-3">
                        {task.status} · {task.project ?? "no project"}
                      </div>
                    </div>
                    {task.pr_url ? (
                      <span className="font-mono text-[11px] text-rust">PR</span>
                    ) : null}
                  </div>
                </button>
              ))}
            </div>
          )}
        </div>
      </div>
      <TaskDetailPanel taskId={selectedTaskId} />
    </div>
  );
}
