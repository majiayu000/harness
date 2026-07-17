import React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { Sidebar, type SidebarSection } from "@/components/Sidebar";
import { TopBar } from "@/components/TopBar";
import { PaletteFab } from "@/components/PaletteFab";
import { DOCS_URL } from "@/lib/links";
import { useWorktrees, useOverview } from "@/lib/queries";
import { apiFetch, runtimeSubmissionPath, TOKEN_KEY } from "@/lib/api";
import type { WorktreeCard } from "@/lib/queries";

function formatDuration(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3_600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3_600)}h`;
  return `${Math.floor(seconds / 86_400)}d`;
}

function phaseLabel(phase: string): string {
  return phase.replace(/_/g, " ");
}

function branchLabel(card: WorktreeCard): string {
  return card.branch;
}

function statusColor(status: string): string {
  switch (status) {
    case "failed":
      return "text-danger border-danger/40";
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

function openStream(submissionId: string): void {
  const tok = (globalThis.sessionStorage?.getItem?.(TOKEN_KEY) ?? "").trim();
  const base = runtimeSubmissionPath(submissionId, "stream");
  const url = tok ? `${base}?token=${encodeURIComponent(tok)}` : base;
  window.open(url, "_blank", "noreferrer");
}

interface CardProps {
  card: WorktreeCard;
  onCancel: (card: WorktreeCard) => void;
  cancelling: boolean;
}

function WorktreeCardItem({ card, onCancel, cancelling }: CardProps) {
  const pct = card.maxTurns != null && card.maxTurns > 0 ? Math.round((card.turn / card.maxTurns) * 100) : null;
  const failed = card.status === "failed";

  return (
    <div
      className={`border border-line rounded-[4px] bg-bg-1 flex flex-col gap-0 overflow-hidden ${
        failed ? "border-l-2 border-l-danger" : ""
      }`}
    >
      <div className="px-4 py-3 flex items-center gap-2 border-b border-line min-w-0">
        <span
          className={`font-mono text-[10.5px] px-1.5 py-[1px] border rounded-[10px] flex-none ${statusColor(card.status)}`}
        >
          {card.status}
        </span>
        <span className="font-mono text-[12px] text-ink font-medium truncate" title={card.branch}>
          {branchLabel(card)}
        </span>
        <span className="font-mono text-[10.5px] text-ink-3 flex-none">
          {formatDuration(card.durationSecs)}
        </span>
        {card.prUrl && (
          <a
            href={card.prUrl}
            target="_blank"
            rel="noreferrer"
            className="ml-auto font-mono text-[10.5px] px-1.5 py-[1px] border border-rust/40 text-rust rounded-[3px] flex-none hover:bg-rust/5"
          >
            PR
          </a>
        )}
        {!card.prUrl && <span className="ml-auto" />}
      </div>

      <div className="px-4 py-3 border-b border-line min-h-[74px]">
        <p
          className="m-0 text-[13px] leading-5 text-ink-2 line-clamp-2"
          title={card.description ?? undefined}
        >
          {card.description ?? "No description"}
        </p>
        <span className="inline-flex mt-2 font-mono text-[10.5px] px-1.5 py-[1px] border border-line-2 text-ink-3 rounded-[3px]">
          {phaseLabel(card.phase)}
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

      <div className="px-4 py-2.5 flex items-center gap-2 min-w-0">
        <span
          className="font-mono text-[11.5px] text-ink-3 truncate mr-auto"
          title={card.workspacePath}
        >
          {card.pathShort}
        </span>
        <button
          type="button"
          disabled={!card.runtimeSubmissionId}
          title={card.runtimeSubmissionId ? undefined : "Runtime submission id unavailable"}
          onClick={() => {
            if (card.runtimeSubmissionId) openStream(card.runtimeSubmissionId);
          }}
          className="font-mono text-[11.5px] px-3 py-1 border border-line-2 text-ink-2 rounded-[3px] hover:bg-bg-2 hover:text-ink disabled:opacity-50 disabled:cursor-not-allowed"
        >
          Logs
        </button>
        <button
          type="button"
          disabled={cancelling || !card.runtimeWorkflowId}
          title={card.runtimeWorkflowId ? undefined : "Workflow runtime id unavailable"}
          onClick={() => onCancel(card)}
          className="font-mono text-[11.5px] px-3 py-1 border border-danger/40 text-danger rounded-[3px] hover:bg-danger/5 disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {cancelling ? "Cancelling..." : "Cancel"}
        </button>
      </div>
    </div>
  );
}

function cancelStateKey(card: WorktreeCard) {
  return card.runtimeWorkflowId ?? card.taskId;
}

export function Worktrees() {
  const { cards, isLoading, error } = useWorktrees();
  const { data: overview } = useOverview();
  const queryClient = useQueryClient();

  const [cancelling, setCancelling] = React.useState<Set<string>>(new Set());
  const [cancelError, setCancelError] = React.useState<string | null>(null);

  const handleCancel = async (card: WorktreeCard) => {
    const cancelKey = cancelStateKey(card);
    setCancelError(null);
    setCancelling((prev) => new Set(prev).add(cancelKey));
    try {
      if (card.runtimeWorkflowId) {
        await apiFetch("/api/workflows/runtime/cancel", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ workflow_id: card.runtimeWorkflowId }),
        });
      } else {
        throw new Error("Workflow runtime id unavailable; cancellation was not sent");
      }
    } catch (err) {
      setCancelError(err instanceof Error ? err.message : "Cancel failed");
    } finally {
      const refreshes = [
        queryClient.invalidateQueries({ queryKey: ["worktrees"] }),
        queryClient.invalidateQueries({ queryKey: ["tasks"] }),
        queryClient.invalidateQueries({ queryKey: ["workflow-runtime-tree"] }),
      ];
      const refreshResults = await Promise.allSettled(refreshes);
      for (const result of refreshResults) {
        if (result.status === "rejected") {
          console.error("Failed to refresh worktree data after cancellation", result.reason);
        }
      }
      setCancelling((prev) => {
        const next = new Set(prev);
        next.delete(cancelKey);
        return next;
      });
    }
  };

  const sections: SidebarSection[] = [
    {
      label: "System",
      items: [
        { id: "overview", label: "Overview", href: "/overview" },
        { id: "usage", label: "Usage", href: "/usage" },
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
      items: [{ id: "docs", label: "Docs", href: DOCS_URL }],
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
              <div
                role="alert"
                className="mb-4 px-3 py-2 border border-danger/40 text-danger font-mono text-[12px] rounded-[3px] bg-danger/5"
              >
                {cancelError}
              </div>
            )}
            {!isLoading && !error && cards.length === 0 ? (
              <div className="flex flex-col items-center justify-center gap-3 py-24 text-ink-3">
                <span className="font-mono text-[13px]">No active worktrees</span>
                <Link
                  to="/dashboard?tab=submit"
                  className="font-mono text-[11.5px] px-3 py-1 border border-line-2 text-ink-2 rounded-[3px] hover:bg-bg-2 hover:text-ink"
                >
                  Submit task
                </Link>
              </div>
            ) : (
              <div className="grid grid-cols-[repeat(auto-fill,minmax(340px,1fr))] gap-4">
                {cards.map((card) => (
                  <WorktreeCardItem
                    key={card.taskId}
                    card={card}
                    onCancel={handleCancel}
                    cancelling={cancelling.has(cancelStateKey(card))}
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
