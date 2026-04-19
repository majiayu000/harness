import { useState } from "react";
import { useDashboard } from "@/lib/queries";

interface Props {
  projectFilter?: string | null;
}

export function History({ projectFilter }: Props) {
  const { data } = useDashboard();
  const [filter, setFilter] = useState<"all" | "done" | "failed">("all");
  const [query, setQuery] = useState("");
  const scopedProject = projectFilter ? data?.projects.find((p) => p.id === projectFilter) : null;
  const totalDone = scopedProject
    ? scopedProject.tasks.done
    : (data?.projects.reduce((a, p) => a + p.tasks.done, 0) ?? 0);
  const totalFailed = scopedProject
    ? scopedProject.tasks.failed
    : (data?.projects.reduce((a, p) => a + p.tasks.failed, 0) ?? 0);

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
        <div className="mt-3 text-ink-4 text-[11px]">
          Task list endpoint is /tasks; cursor-based pagination TBD in a separate PR.
        </div>
      </div>
    </div>
  );
}
