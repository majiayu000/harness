import { useDashboard } from "@/lib/queries";

const COLUMNS: Array<{ key: string; label: string }> = [
  { key: "pending", label: "Pending" },
  { key: "implementing", label: "Implementing" },
  { key: "agent_review", label: "Agent Review" },
  { key: "waiting", label: "Waiting" },
  { key: "reviewing", label: "Reviewing" },
];

export function Active() {
  const { data } = useDashboard();
  const projects = data?.projects ?? [];
  const byStatus: Record<string, number> = {
    pending: projects.reduce((a, p) => a + p.tasks.queued, 0),
    implementing: projects.reduce((a, p) => a + p.tasks.running, 0),
    agent_review: 0,
    waiting: 0,
    reviewing: 0,
  };

  return (
    <div className="grid gap-3" style={{ gridTemplateColumns: `repeat(${COLUMNS.length}, 1fr)` }}>
      {COLUMNS.map((col) => (
        <div key={col.key} className="border border-line bg-bg-1 min-h-[200px]">
          <div className="px-3 py-2 border-b border-line font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 flex justify-between">
            <span>{col.label}</span>
            <span className="text-ink-2">{byStatus[col.key] ?? 0}</span>
          </div>
          <div className="p-2 text-ink-4 font-mono text-[11px]">
            live task cards land here — harness-server doesn't yet emit per-task status broken down by kanban column
          </div>
        </div>
      ))}
    </div>
  );
}
