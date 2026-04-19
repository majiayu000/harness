import { useState } from "react";
import { useTasks } from "@/lib/queries";
import { TaskSlideover } from "@/components/TaskSlideover";
import type { Task } from "@/types";

interface Column {
  key: string;
  label: string;
  /** Status strings from `GET /tasks` that belong in this column. */
  statuses: string[];
}

/**
 * Columns used by the dashboard kanban. Keep in sync with harness-core's
 * `TaskStatus` enum — any status not listed here is collected under the
 * catch-all "Other" column so operators always see every task.
 */
const COLUMNS: Column[] = [
  { key: "pending", label: "Pending", statuses: ["pending", "queued"] },
  { key: "implementing", label: "Implementing", statuses: ["implementing", "running", "triage", "plan"] },
  { key: "agent_review", label: "Agent Review", statuses: ["agent_review", "reviewing_agent"] },
  { key: "waiting", label: "Waiting", statuses: ["waiting"] },
  { key: "reviewing", label: "Reviewing", statuses: ["reviewing"] },
];

const TERMINAL_STATUSES = new Set(["done", "failed", "cancelled"]);

function columnOf(status: string): string {
  for (const c of COLUMNS) {
    if (c.statuses.includes(status)) return c.key;
  }
  return "other";
}

function TaskCard({ task, onClick }: { task: Task; onClick: () => void }) {
  const title = task.description?.trim() || task.repo || task.id.slice(0, 8);
  return (
    <div
      onClick={onClick}
      className="border border-line bg-bg px-2.5 py-2 mb-2 last:mb-0 hover:border-line-3 transition-colors cursor-pointer"
    >
      <div className="text-[12.5px] text-ink leading-snug line-clamp-2" title={title}>
        {title}
      </div>
      <div className="mt-1.5 flex items-center justify-between gap-2 font-mono text-[10px] text-ink-3">
        <span className="truncate">{task.repo ?? "—"}</span>
        {task.turn > 0 && <span>turn {task.turn}</span>}
      </div>
      {task.pr_url && (
        <a
          href={task.pr_url}
          target="_blank"
          rel="noreferrer"
          onClick={(e) => e.stopPropagation()}
          className="mt-1 block font-mono text-[10px] text-rust hover:underline truncate"
        >
          {task.pr_url.replace(/^https:\/\/github\.com\//, "")}
        </a>
      )}
    </div>
  );
}

export function Active() {
  const [selectedTaskId, setSelectedTaskId] = useState<string | null>(null);
  const { data, isLoading, isError } = useTasks();

  const active = (data ?? []).filter((t) => !TERMINAL_STATUSES.has(t.status));
  const grouped: Record<string, Task[]> = {};
  for (const c of COLUMNS) grouped[c.key] = [];
  const other: Task[] = [];
  for (const t of active) {
    const col = columnOf(t.status);
    if (col === "other") other.push(t);
    else grouped[col].push(t);
  }
  const showOther = other.length > 0;

  return (
    <>
      <div
        className="grid gap-3"
        style={{ gridTemplateColumns: `repeat(${COLUMNS.length + (showOther ? 1 : 0)}, 1fr)` }}
      >
        {COLUMNS.map((col) => {
          const rows = grouped[col.key];
          return (
            <div key={col.key} className="border border-line bg-bg-1 min-h-[200px] flex flex-col">
              <div className="px-3 py-2 border-b border-line font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 flex justify-between flex-none">
                <span>{col.label}</span>
                <span className="text-ink-2">{rows.length}</span>
              </div>
              <div className="p-2 flex-1 overflow-auto">
                {rows.length === 0 && (
                  <div className="text-ink-4 font-mono text-[11px] p-1">
                    {isLoading ? "loading…" : isError ? "error" : "—"}
                  </div>
                )}
                {rows.map((t) => (
                  <TaskCard key={t.id} task={t} onClick={() => setSelectedTaskId(t.id)} />
                ))}
              </div>
            </div>
          );
        })}
        {showOther && (
          <div className="border border-line bg-bg-1 min-h-[200px] flex flex-col">
            <div className="px-3 py-2 border-b border-line font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 flex justify-between flex-none">
              <span>Other</span>
              <span className="text-ink-2">{other.length}</span>
            </div>
            <div className="p-2 flex-1 overflow-auto">
              {other.map((t) => (
                <TaskCard key={t.id} task={t} onClick={() => setSelectedTaskId(t.id)} />
              ))}
            </div>
          </div>
        )}
      </div>
      <TaskSlideover taskId={selectedTaskId} onClose={() => setSelectedTaskId(null)} />
    </>
  );
}
