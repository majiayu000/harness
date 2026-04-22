import React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Sidebar, type SidebarSection } from "@/components/Sidebar";
import { TopBar } from "@/components/TopBar";
import { PaletteFab } from "@/components/PaletteFab";
import { useOverview, useWorktrees } from "@/lib/queries";
import { apiFetch, TOKEN_KEY } from "@/lib/api";
import { formatDurationShort } from "@/lib/format";
import type { Worktree } from "@/types";

function titleCase(value: string): string {
  return value
    .split("_")
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(" ");
}

function statusOrder(status: string): number {
  switch (status) {
    case "failed":
      return 0;
    case "agent_review":
    case "reviewing":
      return 1;
    case "implementing":
      return 2;
    case "pending":
    case "awaiting_deps":
    case "waiting":
      return 3;
    default:
      return 4;
  }
}

function sortWorktrees(worktrees: Worktree[]): Worktree[] {
  return [...worktrees].sort((a, b) => {
    const byStatus = statusOrder(a.status) - statusOrder(b.status);
    if (byStatus !== 0) return byStatus;
    return a.task_id.localeCompare(b.task_id);
  });
}

function statusColor(status: string): string {
  switch (status) {
    case "failed":
      return "text-danger border-danger/40 bg-danger/5";
    case "implementing":
    case "planner_generating":
      return "text-ok border-ok/40 bg-ok/5";
    case "review_generating":
    case "agent_review":
    case "reviewing":
      return "text-rust border-rust/40 bg-rust/5";
    case "review_waiting":
    case "planner_waiting":
    case "pending":
    case "awaiting_deps":
    case "waiting":
      return "text-sand border-sand/40 bg-sand/10";
    default:
      return "text-ink-3 border-line-2 bg-bg-2";
  }
}

function openStream(taskId: string): void {
  const tok = (globalThis.sessionStorage?.getItem?.(TOKEN_KEY) ?? "").trim();
  const base = `/tasks/${taskId}/stream`;
  const url = tok ? `${base}?token=${encodeURIComponent(tok)}` : base;
  window.open(url, "_blank", "noreferrer");
}

interface CardProps {
  worktree: Worktree;
  onCancel: (taskId: string) => void;
  cancelling: boolean;
}

function WorktreeCardItem({ worktree, onCancel, cancelling }: CardProps) {
  const pct = worktree.max_turns != null && worktree.max_turns > 0
    ? Math.min(100, Math.round((worktree.turn / worktree.max_turns) * 100))
    : null;
  const leftBorder = worktree.status === "failed" ? "border-l-2 border-l-danger" : "border-l-2 border-l-transparent";

  return (
    <article className={`border border-line rounded-[4px] bg-bg-1 overflow-hidden ${leftBorder}`}>
      <div className="px-4 py-3 border-b border-line space-y-3">
        <div className="flex items-center gap-2">
          <span className="font-mono text-[10.5px] px-1.5 py-[1px] border border-line-2 text-ink-3 rounded-[3px]">
            {worktree.repo}
          </span>
          <span
            className={`font-mono text-[10.5px] px-1.5 py-[1px] border rounded-[10px] ${statusColor(worktree.status)}`}
          >
            {titleCase(worktree.status)}
          </span>
          <span className="ml-auto font-mono text-[10.5px] text-ink-4 uppercase tracking-[0.1em]">
            {titleCase(worktree.phase)}
          </span>
        </div>
        <div>
          <div className="text-[15px] leading-6 text-ink font-medium truncate">{worktree.description}</div>
          <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 font-mono text-[11px] text-ink-3">
            <span>{worktree.task_id}</span>
            <span>{worktree.branch}</span>
            <span title={worktree.workspace_path}>{worktree.path_short}</span>
          </div>
        </div>
      </div>

      <div className="px-4 py-3 border-b border-line grid grid-cols-3 gap-3">
        <div>
          <div className="font-mono text-[10px] text-ink-4 uppercase tracking-[0.1em]">Project</div>
          <div className="font-mono text-[12px] text-ink">{worktree.project}</div>
        </div>
        <div>
          <div className="font-mono text-[10px] text-ink-4 uppercase tracking-[0.1em]">Age</div>
          <div className="font-mono text-[12px] text-ink">{formatDurationShort(worktree.duration_secs)}</div>
        </div>
        <div>
          <div className="font-mono text-[10px] text-ink-4 uppercase tracking-[0.1em]">Source</div>
          <div className="font-mono text-[12px] text-ink truncate" title={worktree.source_repo}>
            {worktree.source_repo}
          </div>
        </div>
      </div>

      {pct != null && (
        <div className="px-4 py-3 border-b border-line">
          <div className="flex items-center gap-2 mb-1">
            <span className="font-mono text-[10.5px] text-ink-3">
              turn {worktree.turn}/{worktree.max_turns}
            </span>
            <span className="ml-auto font-mono text-[10.5px] text-ink-3">{pct}%</span>
          </div>
          <div className="h-[3px] bg-bg-2 rounded-full overflow-hidden">
            <div className="h-full bg-rust rounded-full" style={{ width: `${pct}%` }} />
          </div>
        </div>
      )}

      <div className="px-4 py-2.5 flex items-center gap-2">
        <button
          type="button"
          onClick={() => openStream(worktree.task_id)}
          className="font-mono text-[11.5px] px-3 py-1 border border-line-2 text-ink-2 rounded-[3px] hover:bg-bg-2 hover:text-ink"
        >
          Logs
        </button>
        {worktree.pr_url ? (
          <a
            href={worktree.pr_url}
            target="_blank"
            rel="noreferrer"
            className="font-mono text-[11.5px] px-3 py-1 border border-line-2 text-ink-2 rounded-[3px] hover:bg-bg-2 hover:text-ink"
          >
            PR
          </a>
        ) : (
          <button
            type="button"
            disabled
            title="PR not created yet"
            className="font-mono text-[11.5px] px-3 py-1 border border-line-2 text-ink-4 rounded-[3px] cursor-not-allowed"
          >
            PR
          </button>
        )}
        <button
          type="button"
          disabled={cancelling}
          onClick={() => onCancel(worktree.task_id)}
          className="ml-auto font-mono text-[11.5px] px-3 py-1 border border-danger/40 text-danger rounded-[3px] hover:bg-danger/5 disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {cancelling ? "Cancelling…" : "Cancel"}
        </button>
      </div>
    </article>
  );
}

export function Worktrees() {
  const { data: worktrees = [], isLoading, error } = useWorktrees();
  const { data: overview } = useOverview();
  const queryClient = useQueryClient();

  const [cancelling, setCancelling] = React.useState<Set<string>>(new Set());
  const [cancelError, setCancelError] = React.useState<string | null>(null);

  const handleCancel = async (taskId: string) => {
    setCancelError(null);
    setCancelling((prev) => new Set(prev).add(taskId));
    try {
      await apiFetch(`/tasks/${taskId}/cancel`, { method: "POST" });
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["worktrees"] }),
        queryClient.invalidateQueries({ queryKey: ["tasks"] }),
      ]);
    } catch (err) {
      setCancelError(err instanceof Error ? err.message : "Cancel failed");
    } finally {
      setCancelling((prev) => {
        const next = new Set(prev);
        next.delete(taskId);
        return next;
      });
    }
  };

  const sortedWorktrees = sortWorktrees(worktrees);

  const sections: SidebarSection[] = [
    {
      label: "System",
      items: [
        { id: "overview", label: "Overview", href: "/overview" },
        { id: "projects", label: "Projects", href: "/overview#projects", count: overview?.projects.length },
        { id: "runtimes", label: "Runtimes", href: "/overview#runtimes", count: overview?.runtimes.length },
        { id: "observability", label: "Observability", href: "/overview#observability" },
      ],
    },
    {
      label: "Fleet",
      items: [
        { id: "tasks", label: "All tasks", href: "/overview#projects", count: overview?.kpi.active_tasks },
        { id: "worktrees", label: "Worktrees", href: "/worktrees", active: true, count: sortedWorktrees.length },
      ],
    },
    {
      label: "Reference",
      items: [{ id: "docs", label: "Docs", href: "/" }],
    },
  ];

  return (
    <div className="grid grid-cols-[240px_1fr] h-screen overflow-hidden">
      <Sidebar env="local" sections={sections} />
      <main className="flex flex-col min-h-0 min-w-0">
        <TopBar
          breadcrumb={[{ label: "fleet" }, { label: "worktrees", current: true }]}
          searchPlaceholder="Search worktrees…"
        />
        <div className="flex-1 overflow-auto min-h-0">
          <div className="px-6 py-4.5 border-b border-line bg-bg flex items-center gap-4">
            <h1 className="m-0 text-xl font-medium tracking-[-0.01em]">
              Fleet <em className="font-serif italic text-rust font-normal">worktrees</em>
            </h1>
            <span className="font-mono text-[12px] text-ink-3 ml-1">
              {isLoading ? "loading…" : `${sortedWorktrees.length} active`}
            </span>
          </div>

          <div className="p-6">
            {error && (
              <div className="mb-4 px-3 py-2 border border-danger/40 text-danger font-mono text-[12px] rounded-[3px] bg-danger/5">
                Failed to load worktrees: {error.message}
              </div>
            )}
            {cancelError && (
              <div className="mb-4 px-3 py-2 border border-danger/40 text-danger font-mono text-[12px] rounded-[3px] bg-danger/5">
                {cancelError}
              </div>
            )}
            {!isLoading && !error && sortedWorktrees.length === 0 ? (
              <div className="border border-dashed border-line-2 rounded-[4px] bg-bg-1 px-6 py-16 text-center">
                <div className="font-mono text-[13px] text-ink-3">No active worktrees</div>
                <p className="mt-2 mb-5 text-sm text-ink-3">
                  Submit a task to create a dedicated workspace and track it here.
                </p>
                <a
                  href="/dashboard?tab=submit"
                  className="inline-flex items-center justify-center font-mono text-[11.5px] px-3 py-1 border border-line-2 text-ink-2 rounded-[3px] hover:bg-bg-2 hover:text-ink"
                >
                  Open submit tab
                </a>
              </div>
            ) : (
              <div className="grid grid-cols-[repeat(auto-fill,minmax(340px,1fr))] gap-4">
                {sortedWorktrees.map((worktree) => (
                  <WorktreeCardItem
                    key={worktree.task_id}
                    worktree={worktree}
                    onCancel={handleCancel}
                    cancelling={cancelling.has(worktree.task_id)}
                  />
                ))}
              </div>
            )}
          </div>
        </div>
      </main>
      <PaletteFab />
    </div>
  );
}
