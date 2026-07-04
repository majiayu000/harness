import { useEffect, useMemo, useState } from "react";
import { relativeAgo } from "@/lib/format";
import { useAllTasks, useDashboard } from "@/lib/queries";
import type { Task } from "@/types";

interface Props {
  projectFilter?: string | null;
}

const PAGE_SIZE = 20;
const TERMINAL_STATUSES = new Set(["done", "failed", "cancelled"]);
type HistoryFilter = "all" | "done" | "failed" | "stalled";

function timestampValue(value: string | null | undefined): number {
  if (!value) return 0;
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? 0 : parsed;
}

function statusLabel(status: string): string {
  if (status === "done") return "Done";
  if (status === "failed") return "Failed";
  if (status === "cancelled") return "Cancelled";
  return status;
}

function taskStatusLabel(task: Task): string {
  if (task.terminal?.classification === "stalled") return "Stalled";
  return statusLabel(task.status);
}

function statusClass(status: string): string {
  if (status === "done") return "border-mint/40 bg-mint/10 text-mint";
  if (status === "failed") return "border-rust/40 bg-rust/10 text-rust";
  if (status === "cancelled") return "border-line-3 bg-bg text-ink-3";
  return "border-line bg-bg text-ink-2";
}

function taskStatusClass(task: Task): string {
  if (task.terminal?.classification === "stalled") return "border-warn/40 bg-warn/10 text-warn";
  return statusClass(task.status);
}

function taskTitle(task: Task): string {
  const title = task.description?.trim();
  if (title) return title;
  if (task.repo) return task.repo;
  return task.id.slice(0, 8);
}

function formatReason(value: string): string {
  return value.replace(/_/g, " ");
}

function terminalReason(task: Task): string | null {
  if (task.terminal?.classification !== "stalled") return null;
  const parts = [
    task.terminal.reason ? formatReason(task.terminal.reason) : null,
    task.terminal.waiting_on ? `waiting on ${formatReason(task.terminal.waiting_on)}` : null,
  ].filter(Boolean);
  return parts.length > 0 ? parts.join(" · ") : "stalled";
}

function prLabel(prUrl: string): string {
  const match = prUrl.match(/\/pull\/(\d+)(?:$|[/?#])/);
  return match ? `PR #${match[1]}` : prUrl.replace(/^https:\/\/github\.com\//, "");
}

function matchesQuery(task: Task, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  return [task.description, task.repo, task.pr_url, task.terminal?.reason, task.terminal?.waiting_on].some((value) =>
    value?.toLowerCase().includes(q),
  );
}

function sortHistoryTasks(tasks: Task[]): Task[] {
  return [...tasks].sort((a, b) => {
    const byTime = timestampValue(b.created_at) - timestampValue(a.created_at);
    if (byTime !== 0) return byTime;
    return a.id.localeCompare(b.id);
  });
}

export function History({ projectFilter }: Props) {
  const [filter, setFilter] = useState<HistoryFilter>("all");
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(1);
  const { data: dashboard } = useDashboard();
  const resolvedRoot = projectFilter
    ? (dashboard?.projects.find((p) => p.id === projectFilter)?.root ?? projectFilter)
    : null;
  const { data, isLoading, isError } = useAllTasks({
    status: "done,failed,cancelled",
    limit: 500,
    project_id: resolvedRoot ?? undefined,
  });
  const submitHref = projectFilter
    ? `/dashboard?tab=submit&project=${encodeURIComponent(projectFilter)}`
    : "/dashboard?tab=submit";

  const rows = useMemo(() => {
    const statusFiltered =
      data?.data.filter((task) => {
        if (!TERMINAL_STATUSES.has(task.status)) return false;
        if (filter === "stalled") return task.terminal?.classification === "stalled" && matchesQuery(task, query);
        if (filter !== "all" && task.status !== filter) return false;
        return matchesQuery(task, query);
      }) ?? [];
    return sortHistoryTasks(statusFiltered);
  }, [data?.data, filter, query]);

  useEffect(() => {
    setPage(1);
  }, [filter, query, resolvedRoot]);

  const totalPages = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));
  const currentPage = Math.min(page, totalPages);
  const pageRows = rows.slice((currentPage - 1) * PAGE_SIZE, currentPage * PAGE_SIZE);

  return (
    <div>
      <div className="flex gap-2 mb-4">
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search description, repo, PR…"
          className="flex-1 h-[30px] bg-bg-1 border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
        />
        <select
          value={filter}
          onChange={(e) => setFilter(e.target.value as HistoryFilter)}
          className="h-[30px] bg-bg-1 border border-line-2 text-ink font-mono text-[12px] px-2 rounded-[3px]"
        >
          <option value="all">All</option>
          <option value="done">Done</option>
          <option value="failed">Failed</option>
          <option value="stalled">Stalled</option>
        </select>
      </div>

      <div className="border border-line bg-bg-1">
        <div className="flex items-center justify-between border-b border-line px-3 py-2 font-mono text-[11px] text-ink-3">
          <span>{rows.length} terminal tasks</span>
          <span>
            page {currentPage}/{totalPages}
          </span>
        </div>

        {isLoading ? (
          <div className="px-3 py-8 text-center font-mono text-[12px] text-ink-3">Loading history…</div>
        ) : isError ? (
          <div className="px-3 py-8 text-center font-mono text-[12px] text-rust">History unavailable</div>
        ) : pageRows.length === 0 ? (
          <div className="flex flex-col items-center gap-3 px-3 py-10 text-center">
            <div className="font-mono text-[12px] text-ink-3">No terminal tasks found.</div>
            <a
              href={submitHref}
              className="border border-line-2 bg-bg px-3 py-1.5 font-mono text-[12px] text-ink hover:border-line-3 transition-colors"
            >
              Submit task
            </a>
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[760px] border-collapse text-left">
              <thead className="border-b border-line bg-bg">
                <tr className="font-mono text-[10px] uppercase tracking-[0.08em] text-ink-4">
                  <th className="w-[90px] px-3 py-2 font-normal">Time</th>
                  <th className="w-[110px] px-3 py-2 font-normal">Status</th>
                  <th className="px-3 py-2 font-normal">Task</th>
                  <th className="w-[180px] px-3 py-2 font-normal">Repo</th>
                  <th className="w-[120px] px-3 py-2 font-normal">PR</th>
                  <th className="w-[80px] px-3 py-2 text-right font-normal">Turns</th>
                </tr>
              </thead>
              <tbody>
                {pageRows.map((task) => (
                  <tr key={task.id} className="border-b border-line last:border-b-0">
                    <td className="px-3 py-2 font-mono text-[11px] text-ink-3">
                      {task.created_at ? relativeAgo(task.created_at) : "—"}
                    </td>
                    <td className="px-3 py-2">
                      <span className={`inline-block border px-1.5 py-[1px] font-mono text-[10px] ${taskStatusClass(task)}`}>
                        {taskStatusLabel(task)}
                      </span>
                    </td>
                    <td className="px-3 py-2">
                      <div className="line-clamp-2 text-[12.5px] leading-snug text-ink" title={taskTitle(task)}>
                        {taskTitle(task)}
                      </div>
                      <div className="mt-1 font-mono text-[10px] text-ink-4">{task.id.slice(0, 8)}</div>
                      {terminalReason(task) && (
                        <div className="mt-1 line-clamp-1 font-mono text-[10px] text-warn" title={terminalReason(task) ?? undefined}>
                          {terminalReason(task)}
                        </div>
                      )}
                    </td>
                    <td className="px-3 py-2 font-mono text-[11px] text-ink-3">
                      <span className="block truncate" title={task.repo ?? undefined}>
                        {task.repo ?? "—"}
                      </span>
                    </td>
                    <td className="px-3 py-2 font-mono text-[11px]">
                      {task.pr_url ? (
                        <a href={task.pr_url} target="_blank" rel="noreferrer" className="text-rust hover:underline">
                          {prLabel(task.pr_url)}
                        </a>
                      ) : (
                        <span className="text-ink-4">—</span>
                      )}
                    </td>
                    <td className="px-3 py-2 text-right font-mono text-[11px] text-ink-3">{task.turn}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

        <div className="flex items-center justify-end gap-2 border-t border-line px-3 py-2">
          <button
            type="button"
            disabled={currentPage <= 1}
            onClick={() => setPage((value) => Math.max(1, value - 1))}
            className="h-[28px] border border-line bg-bg px-2 font-mono text-[11px] text-ink-2 hover:border-line-3 hover:text-ink disabled:opacity-40 disabled:cursor-not-allowed"
          >
            Prev
          </button>
          <button
            type="button"
            disabled={currentPage >= totalPages}
            onClick={() => setPage((value) => Math.min(totalPages, value + 1))}
            className="h-[28px] border border-line bg-bg px-2 font-mono text-[11px] text-ink-2 hover:border-line-3 hover:text-ink disabled:opacity-40 disabled:cursor-not-allowed"
          >
            Next
          </button>
        </div>
      </div>
    </div>
  );
}
