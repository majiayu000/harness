import { useState } from "react";
import { useDashboard, useTasks } from "@/lib/queries";
import type { Task } from "@/types";

interface Props {
  projectFilter?: string | null;
}

export function History({ projectFilter }: Props) {
  const { data } = useDashboard();
  const { data: tasks } = useTasks();
  const [filter, setFilter] = useState<"all" | "done" | "failed">("all");
  const [query, setQuery] = useState("");
  const scopedProject = projectFilter ? (data?.projects.find((p) => p.id === projectFilter) ?? null) : null;
  const totalDone = projectFilter
    ? (scopedProject?.tasks.done ?? 0)
    : (data?.projects.reduce((a, p) => a + p.tasks.done, 0) ?? 0);
  const totalFailed = projectFilter
    ? (scopedProject?.tasks.failed ?? 0)
    : (data?.projects.reduce((a, p) => a + p.tasks.failed, 0) ?? 0);
  const normalizedQuery = query.trim().toLowerCase();
  const rows = (tasks ?? [])
    .filter((task) => !projectFilter || task.project === scopedProject?.root || task.project === projectFilter)
    .filter((task) => filter === "all" || task.status === filter)
    .filter((task) => {
      if (!normalizedQuery) return true;
      return [
        task.id,
        task.description,
        task.repo,
        task.status,
        task.phase,
        task.active_phase,
      ]
        .filter(Boolean)
        .some((value) => value!.toLowerCase().includes(normalizedQuery));
    })
    .sort((a, b) => {
      const aTs = Date.parse(a.phase_started_at ?? a.created_at ?? "") || 0;
      const bTs = Date.parse(b.phase_started_at ?? b.created_at ?? "") || 0;
      return bTs - aTs;
    })
    .slice(0, 50);

  return (
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
      <div className="border border-line bg-bg-1 p-4 font-mono text-[12px] text-ink-3">
        done {totalDone} · failed {totalFailed} · filter {filter} · query {query || "(none)"}
      </div>
      <div className="mt-4 border border-line bg-bg-1">
        {rows.length === 0 ? (
          <div className="p-4 font-mono text-[12px] text-ink-4">No matching tasks</div>
        ) : (
          rows.map((task) => <HistoryRow key={task.id} task={task} />)
        )}
      </div>
    </div>
  );
}

function formatTimestamp(ts: string | null): string {
  if (!ts) return "—";
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toLocaleString([], {
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function HistoryRow({ task }: { task: Task }) {
  const title = task.description?.trim() || task.repo || task.id.slice(0, 8);
  const phaseLabel = task.active_phase ?? task.phase ?? "unknown";
  const phaseTs = task.phase_started_at ?? task.created_at;

  return (
    <div className="grid grid-cols-[minmax(0,2.2fr)_110px_120px_150px] gap-3 px-4 py-3 border-b border-line last:border-b-0">
      <div className="min-w-0">
        <div className="text-[12.5px] text-ink truncate" title={title}>
          {title}
        </div>
        <div className="mt-1 font-mono text-[10px] text-ink-4">
          {task.repo ?? task.id}
        </div>
      </div>
      <div className="font-mono text-[11px] text-ink-3 uppercase">{task.status}</div>
      <div className="font-mono text-[11px] text-ink-3 uppercase">
        {phaseLabel}
        {task.agent_active ? " active" : ""}
      </div>
      <div className="font-mono text-[11px] text-ink-4">{formatTimestamp(phaseTs)}</div>
    </div>
  );
}
