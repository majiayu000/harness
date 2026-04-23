import { useTasks, useDashboard } from "@/lib/queries";
import type { Task, WorkflowSummary } from "@/types";

interface Column {
  key: string;
  label: string;
  workflowStates: string[];
  fallbackTaskStatuses: string[];
}

/**
 * Columns used by the dashboard task page.
 *
 * Workflow state is authoritative when present. Task status remains the
 * fallback while older tasks or partially migrated flows lack workflow data.
 */
const COLUMNS: Column[] = [
  {
    key: "pending",
    label: "Pending",
    workflowStates: ["discovered", "scheduled"],
    fallbackTaskStatuses: ["pending", "queued", "awaiting_deps"],
  },
  {
    key: "implementing",
    label: "Implementing",
    workflowStates: ["implementing"],
    fallbackTaskStatuses: ["implementing", "running", "triage", "plan"],
  },
  {
    key: "feedback",
    label: "Feedback",
    workflowStates: ["pr_open", "awaiting_feedback", "addressing_feedback"],
    fallbackTaskStatuses: ["agent_review", "reviewing_agent", "waiting", "reviewing"],
  },
  {
    key: "ready",
    label: "Ready",
    workflowStates: ["ready_to_merge"],
    fallbackTaskStatuses: [],
  },
  {
    key: "blocked",
    label: "Blocked",
    workflowStates: ["blocked", "degraded", "paused"],
    fallbackTaskStatuses: [],
  },
];

const TERMINAL_STATUSES = new Set(["done", "failed", "cancelled"]);

function columnOf(taskStatus: string, workflowState?: string | null): string {
  if (workflowState) {
    for (const c of COLUMNS) {
      if (c.workflowStates.includes(workflowState)) return c.key;
    }
  }
  for (const c of COLUMNS) {
    if (c.fallbackTaskStatuses.includes(taskStatus)) return c.key;
  }
  return "other";
}

function workflowLabel(state: string): string {
  switch (state) {
    case "discovered":
      return "Discovered";
    case "scheduled":
      return "Scheduled";
    case "implementing":
      return "Implementing";
    case "pr_open":
      return "PR Open";
    case "awaiting_feedback":
      return "Awaiting Feedback";
    case "addressing_feedback":
      return "Addressing Feedback";
    case "ready_to_merge":
      return "Ready To Merge";
    case "blocked":
      return "Blocked";
    case "done":
      return "Done";
    case "failed":
      return "Failed";
    case "cancelled":
      return "Cancelled";
    case "idle":
      return "Idle";
    case "polling_intake":
      return "Polling Intake";
    case "planning_batch":
      return "Planning Batch";
    case "dispatching":
      return "Dispatching";
    case "monitoring":
      return "Monitoring";
    case "sweeping_feedback":
      return "Sweeping Feedback";
    case "paused":
      return "Paused";
    case "degraded":
      return "Degraded";
    default:
      return state;
  }
}

function TaskCard({ task, workflow }: { task: Task; workflow?: WorkflowSummary | null }) {
  const title = task.description?.trim() || task.repo || task.id.slice(0, 8);
  return (
    <div className="border border-line bg-bg px-2.5 py-2 mb-2 last:mb-0 hover:border-line-3 transition-colors">
      <div className="text-[12.5px] text-ink leading-snug line-clamp-2" title={title}>
        {title}
      </div>
      {workflow && (
        <div className="mt-1 flex flex-wrap items-center gap-1">
          <span className="border border-line bg-bg-1 px-1.5 py-[1px] font-mono text-[10px] text-ink-2">
            wf {workflowLabel(workflow.state)}
          </span>
          {workflow.pr_number ? (
            <span className="font-mono text-[10px] text-ink-3">PR #{workflow.pr_number}</span>
          ) : null}
          {workflow.force_execute ? (
            <span className="border border-rust/40 bg-rust/10 px-1.5 py-[1px] font-mono text-[10px] text-rust">
              force-execute
            </span>
          ) : null}
        </div>
      )}
      <div className="mt-1.5 flex items-center justify-between gap-2 font-mono text-[10px] text-ink-3">
        <span className="truncate">{task.repo ?? "—"}</span>
        {task.turn > 0 && <span>turn {task.turn}</span>}
      </div>
      {workflow?.plan_concern && (
        <div
          className="mt-1 block font-mono text-[10px] text-rust truncate"
          title={workflow.plan_concern}
        >
          concern: {workflow.plan_concern}
        </div>
      )}
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

interface Props {
  projectFilter?: string | null;
}

export function Active({ projectFilter }: Props) {
  const { data, isLoading, isError } = useTasks();
  const { data: dashboard } = useDashboard();

  const resolvedRoot = projectFilter
    ? (dashboard?.projects.find((p) => p.id === projectFilter)?.root ?? projectFilter)
    : null;

  const active = (data ?? [])
    .filter((t) => !TERMINAL_STATUSES.has(t.status))
    .filter((t) => !resolvedRoot || t.project === resolvedRoot);
  const grouped: Record<string, Task[]> = {};
  for (const c of COLUMNS) grouped[c.key] = [];
  const other: Task[] = [];
  for (const t of active) {
    const workflow = t.workflow ?? null;
    const col = columnOf(t.status, workflow?.state ?? null);
    if (col === "other") other.push(t);
    else grouped[col].push(t);
  }
  const showOther = other.length > 0;

  return (
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
                <TaskCard key={t.id} task={t} workflow={t.workflow ?? null} />
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
                <TaskCard key={t.id} task={t} workflow={t.workflow ?? null} />
              ))}
            </div>
          </div>
      )}
    </div>
  );
}
