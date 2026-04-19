import { useState, useMemo } from "react";
import { useTasks } from "@/lib/queries";
import { relativeAgo } from "@/lib/format";
import type { Task } from "@/types";

const PAGE_SIZE = 20;
const TERMINAL_STATUSES = new Set(["done", "failed", "cancelled"]);

type FilterValue = "all" | "done" | "failed";

function statusPillClass(status: string): string {
  if (status === "done") return "text-[color:var(--color-green)] bg-[color:var(--color-green)]/10";
  if (status === "failed") return "text-rust bg-[color:var(--color-rust)]/10";
  return "text-ink-3 bg-line";
}

export function filterAndPage(
  tasks: Task[],
  filter: FilterValue,
  query: string,
  page: number,
): { rows: Task[]; totalPages: number } {
  const q = query.trim().toLowerCase();
  const filtered = tasks.filter((t) => {
    if (!TERMINAL_STATUSES.has(t.status)) return false;
    if (filter !== "all" && t.status !== filter) return false;
    if (q) {
      const haystack = [t.description, t.repo, t.pr_url].filter(Boolean).join(" ").toLowerCase();
      if (!haystack.includes(q)) return false;
    }
    return true;
  });
  filtered.sort((a, b) => {
    const da = a.created_at ? new Date(a.created_at).getTime() : 0;
    const db = b.created_at ? new Date(b.created_at).getTime() : 0;
    return db - da;
  });
  const totalPages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
  const safePage = Math.min(page, totalPages - 1);
  const rows = filtered.slice(safePage * PAGE_SIZE, safePage * PAGE_SIZE + PAGE_SIZE);
  return { rows, totalPages };
}

function HistoryRow({ task }: { task: Task }) {
  const title = task.description?.trim() || `${task.repo ?? "—"} · ${task.id.slice(0, 8)}`;
  const ago = task.created_at ? relativeAgo(task.created_at) : "—";
  return (
    <tr className="border-b border-line last:border-0 hover:bg-bg transition-colors">
      <td className="px-3 py-2 font-mono text-[10.5px] text-ink-3 whitespace-nowrap">{ago}</td>
      <td className="px-3 py-2">
        <span
          className={`inline-block px-1.5 py-0.5 rounded-[2px] font-mono text-[10px] uppercase tracking-wide ${statusPillClass(task.status)}`}
        >
          {task.status}
        </span>
      </td>
      <td className="px-3 py-2 font-mono text-[11.5px] text-ink max-w-[280px]">
        <span className="line-clamp-1" title={title}>
          {title}
        </span>
      </td>
      <td className="px-3 py-2 font-mono text-[10.5px] text-ink-3 whitespace-nowrap">{task.repo ?? "—"}</td>
      <td className="px-3 py-2">
        {task.pr_url ? (
          <a
            href={task.pr_url}
            target="_blank"
            rel="noreferrer"
            className="font-mono text-[10.5px] text-rust hover:underline truncate max-w-[160px] block"
          >
            {task.pr_url.replace(/^https:\/\/github\.com\//, "")}
          </a>
        ) : (
          <span className="text-ink-4 font-mono text-[10.5px]">—</span>
        )}
      </td>
      <td className="px-3 py-2 font-mono text-[10.5px] text-ink-3 whitespace-nowrap text-right">
        {task.turn > 0 ? task.turn : "—"}
      </td>
    </tr>
  );
}

export function History() {
  const { data, isLoading, isError } = useTasks();
  const [filter, setFilter] = useState<FilterValue>("all");
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(0);

  const { rows, totalPages } = useMemo(
    () => filterAndPage(data ?? [], filter, query, page),
    [data, filter, query, page],
  );

  function handleFilterChange(v: FilterValue) {
    setFilter(v);
    setPage(0);
  }

  function handleQueryChange(v: string) {
    setQuery(v);
    setPage(0);
  }

  return (
    <div>
      <div className="flex gap-2 mb-4">
        <input
          value={query}
          onChange={(e) => handleQueryChange(e.target.value)}
          placeholder="Search history…"
          className="flex-1 h-[30px] bg-bg-1 border border-line-2 px-2.5 text-ink font-mono text-[12px] rounded-[3px]"
        />
        <select
          value={filter}
          onChange={(e) => handleFilterChange(e.target.value as FilterValue)}
          className="h-[30px] bg-bg-1 border border-line-2 text-ink font-mono text-[12px] px-2 rounded-[3px]"
        >
          <option value="all">All</option>
          <option value="done">Done</option>
          <option value="failed">Failed</option>
        </select>
      </div>

      <div className="border border-line bg-bg-1 overflow-x-auto">
        {isLoading && (
          <div className="p-4 font-mono text-[12px] text-ink-4">Loading…</div>
        )}
        {isError && (
          <div className="p-4 font-mono text-[12px] text-rust">Failed to load tasks.</div>
        )}
        {!isLoading && !isError && rows.length === 0 && (
          <div className="p-4 font-mono text-[12px] text-ink-4">No terminal tasks found.</div>
        )}
        {!isLoading && !isError && rows.length > 0 && (
          <table className="w-full border-collapse">
            <thead>
              <tr className="border-b border-line">
                <th className="px-3 py-2 font-mono text-[10px] tracking-[0.1em] uppercase text-ink-3 text-left whitespace-nowrap">
                  Time
                </th>
                <th className="px-3 py-2 font-mono text-[10px] tracking-[0.1em] uppercase text-ink-3 text-left">
                  Status
                </th>
                <th className="px-3 py-2 font-mono text-[10px] tracking-[0.1em] uppercase text-ink-3 text-left">
                  Description
                </th>
                <th className="px-3 py-2 font-mono text-[10px] tracking-[0.1em] uppercase text-ink-3 text-left whitespace-nowrap">
                  Repo
                </th>
                <th className="px-3 py-2 font-mono text-[10px] tracking-[0.1em] uppercase text-ink-3 text-left">
                  PR
                </th>
                <th className="px-3 py-2 font-mono text-[10px] tracking-[0.1em] uppercase text-ink-3 text-right whitespace-nowrap">
                  Turns
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((t) => (
                <HistoryRow key={t.id} task={t} />
              ))}
            </tbody>
          </table>
        )}
      </div>

      {!isLoading && !isError && totalPages > 1 && (
        <div className="flex items-center justify-between mt-3 font-mono text-[11px] text-ink-3">
          <button
            onClick={() => setPage((p) => Math.max(0, p - 1))}
            disabled={page === 0}
            className="px-3 h-[26px] border border-line-2 bg-bg-1 rounded-[3px] disabled:opacity-40 hover:border-line-3 transition-colors"
          >
            ← Prev
          </button>
          <span>
            {page + 1} / {totalPages}
          </span>
          <button
            onClick={() => setPage((p) => Math.min(totalPages - 1, p + 1))}
            disabled={page >= totalPages - 1}
            className="px-3 h-[26px] border border-line-2 bg-bg-1 rounded-[3px] disabled:opacity-40 hover:border-line-3 transition-colors"
          >
            Next →
          </button>
        </div>
      )}
    </div>
  );
}
