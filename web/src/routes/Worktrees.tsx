import React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Sidebar, type SidebarSection } from "@/components/Sidebar";
import { TopBar } from "@/components/TopBar";
import { PaletteFab } from "@/components/PaletteFab";
import { useWorktrees, useOverview } from "@/lib/queries";
import { apiFetch, TOKEN_KEY } from "@/lib/api";
import type { WorktreeCard } from "@/lib/queries";

function fmtPct(v: number | null): string {
  return v != null ? `${v.toFixed(1)}%` : "—";
}

function fmtBytes(v: number | null): string {
  return v != null ? `${(v / 1_073_741_824).toFixed(1)} GB` : "—";
}

function statusColor(status: string): string {
  switch (status) {
    case "implementing":
    case "planner_generating":
      return "text-ok border-ok/40";
    case "review_generating":
      return "text-rust border-rust/40";
    case "review_waiting":
    case "planner_waiting":
    case "pending":
    case "awaiting_deps":
    case "waiting":
      return "text-sand border-sand/40";
    case "agent_review":
    case "reviewing":
      return "text-rust border-rust/40";
    default:
      return "text-ink-3 border-line-2";
  }
}

function openStream(taskId: string): void {
  const tok = (globalThis.sessionStorage?.getItem?.(TOKEN_KEY) ?? "").trim();
  const base = `/tasks/${taskId}/stream`;
  const url = tok ? `${base}?token=${encodeURIComponent(tok)}` : base;
  window.open(url, "_blank", "noreferrer");
}

interface CardProps {
  card: WorktreeCard;
  onCancel: (taskId: string) => void;
  cancelling: boolean;
}

function WorktreeCardItem({ card, onCancel, cancelling }: CardProps) {
  const pct = card.maxTurns != null ? Math.round((card.turn / card.maxTurns) * 100) : null;

  return (
    <div className="border border-line rounded-[4px] bg-bg-1 flex flex-col gap-0 overflow-hidden">
      <div className="px-4 py-3 flex items-center gap-2 border-b border-line">
        <span className="font-mono text-[13px] text-ink font-medium truncate" title={card.pathShort}>
          {card.pathShort}
        </span>
        <span className="font-mono text-[10.5px] px-1.5 py-[1px] border border-line-2 text-ink-3 rounded-[3px] flex-none">
          {card.taskId.slice(0, 8)}
        </span>
        <span className="font-mono text-[10.5px] px-1.5 py-[1px] border border-line-2 text-ink-3 rounded-[3px] flex-none">
          {card.branch}
        </span>
        <span
          className={`ml-auto font-mono text-[10.5px] px-1.5 py-[1px] border rounded-[10px] ${statusColor(card.status)}`}
        >
          {card.status}
        </span>
      </div>

      {pct != null && (
        <div className="px-4 py-2 border-b border-line">
          <div className="flex items-center gap-2 mb-1">
            <span className="font-mono text-[10.5px] text-ink-3">
              turn {card.turn}/{card.maxTurns}
            </span>
            <span className="ml-auto font-mono text-[10.5px] text-ink-3">{pct}%</span>
          </div>
          <div className="h-[3px] bg-bg-2 rounded-full overflow-hidden">
            <div
              className="h-full bg-rust rounded-full"
              style={{ width: `${pct}%` }}
            />
          </div>
        </div>
      )}

      <div className="px-4 py-2 grid grid-cols-3 border-b border-line">
        <div>
          <div className="font-mono text-[10px] text-ink-4 uppercase tracking-[0.1em]">CPU</div>
          <div className="font-mono text-[12px] text-ink">{fmtPct(card.cpuPct)}</div>
        </div>
        <div>
          <div className="font-mono text-[10px] text-ink-4 uppercase tracking-[0.1em]">RAM</div>
          <div className="font-mono text-[12px] text-ink">{fmtPct(card.ramPct)}</div>
        </div>
        <div>
          <div className="font-mono text-[10px] text-ink-4 uppercase tracking-[0.1em]">Disk</div>
          <div className="font-mono text-[12px] text-ink">{fmtBytes(card.diskBytes)}</div>
        </div>
      </div>

      <div className="px-4 py-2.5 flex items-center gap-2">
        <button
          type="button"
          onClick={() => openStream(card.taskId)}
          className="font-mono text-[11.5px] px-3 py-1 border border-line-2 text-ink-2 rounded-[3px] hover:bg-bg-2 hover:text-ink"
        >
          Logs
        </button>
        <button
          type="button"
          disabled
          title="Coming soon"
          className="font-mono text-[11.5px] px-3 py-1 border border-line-2 text-ink-4 rounded-[3px] cursor-not-allowed"
        >
          Shell
        </button>
        <button
          type="button"
          disabled={cancelling}
          onClick={() => onCancel(card.taskId)}
          className="ml-auto font-mono text-[11.5px] px-3 py-1 border border-danger/40 text-danger rounded-[3px] hover:bg-danger/5 disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {cancelling ? "Cancelling…" : "Cancel"}
        </button>
      </div>
    </div>
  );
}

export function Worktrees() {
  const { cards, isLoading, error } = useWorktrees();
  const { data: overview } = useOverview();
  const queryClient = useQueryClient();

  const [cancelling, setCancelling] = React.useState<Set<string>>(new Set());
  const [cancelError, setCancelError] = React.useState<string | null>(null);

  const handleCancel = async (taskId: string) => {
    setCancelError(null);
    setCancelling((prev) => new Set(prev).add(taskId));
    try {
      await apiFetch(`/tasks/${taskId}/cancel`, { method: "POST" });
      await queryClient.invalidateQueries({ queryKey: ["tasks"] });
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
        { id: "worktrees", label: "Worktrees", href: "/worktrees", active: true, count: cards.length },
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
              {isLoading ? "loading…" : `${cards.length} active`}
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
            {!isLoading && !error && cards.length === 0 ? (
              <div className="flex flex-col items-center justify-center py-24 text-ink-3">
                <span className="font-mono text-[13px]">No active worktrees</span>
              </div>
            ) : (
              <div className="grid grid-cols-[repeat(auto-fill,minmax(340px,1fr))] gap-4">
                {cards.map((card) => (
                  <WorktreeCardItem
                    key={card.taskId}
                    card={card}
                    onCancel={handleCancel}
                    cancelling={cancelling.has(card.taskId)}
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
