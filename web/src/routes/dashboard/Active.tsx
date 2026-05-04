import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useTasks, useDashboard, useWorkflowRuntimeTree } from "@/lib/queries";
import { apiFetch } from "@/lib/api";
import { TaskDetailSlideover } from "@/components/TaskDetailSlideover";
import { workflowLabel } from "@/lib/format";
import type {
  Task,
  WorkflowRuntimeCommandNode,
  WorkflowRuntimeDecisionRecord,
  WorkflowRuntimeJob,
  WorkflowRuntimeTreeNode,
  WorkflowRuntimeTreePayload,
  WorkflowSummary,
} from "@/types";

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
    fallbackTaskStatuses: ["implementing", "running", "triaging", "planning", "triage", "plan"],
  },
  {
    key: "planning",
    label: "Planning",
    workflowStates: [],
    fallbackTaskStatuses: ["planner_generating", "planner_waiting"],
  },
  {
    key: "review",
    label: "Review",
    workflowStates: [],
    fallbackTaskStatuses: ["review_generating", "review_waiting"],
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

function shouldShowTask(task: Task): boolean {
  if (task.workflow?.state === "ready_to_merge") return true;
  return !TERMINAL_STATUSES.has(task.status);
}

function fallbackTierLabel(tier?: string | null): string | null {
  if (!tier) return null;
  return `tier ${tier.toUpperCase()}`;
}

function fallbackTriggerLabel(trigger?: string | null): string | null {
  if (!trigger) return null;
  return trigger.replaceAll("_", " ");
}

function commandLabel(command: WorkflowRuntimeCommandNode): string {
  const activity = command.command.command.activity;
  if (typeof activity === "string" && activity.trim()) return activity;
  return command.command.command_type.replaceAll("_", " ");
}

function timestampValue(value?: string | null): number {
  if (!value) return 0;
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? 0 : parsed;
}

function runtimeJobUpdatedAt(job: WorkflowRuntimeJob): number {
  return Math.max(timestampValue(job.updated_at), timestampValue(job.created_at));
}

function latestRuntimeJob(command: WorkflowRuntimeCommandNode): WorkflowRuntimeJob | null {
  return command.runtime_jobs.reduce<WorkflowRuntimeJob | null>((latest, job) => {
    if (!latest) return job;
    return runtimeJobUpdatedAt(job) >= runtimeJobUpdatedAt(latest) ? job : latest;
  }, null);
}

function runtimeJobLabel(command: WorkflowRuntimeCommandNode): string {
  const job = latestRuntimeJob(command);
  if (!job) return `${command.runtime_jobs.length} jobs`;
  const notBefore = job.not_before ? ` - not before ${job.not_before}` : "";
  const latestEvent = job.latest_runtime_event_type ? ` - event ${job.latest_runtime_event_type}` : "";
  const promptDigest = job.prompt_packet_digest ? ` - prompt ${job.prompt_packet_digest.slice(0, 12)}` : "";
  return `${command.runtime_jobs.length} jobs - ${job.status}${notBefore}${latestEvent}${promptDigest}`;
}

function runtimeMergeWorkflowId(workflow?: WorkflowSummary | null): string | null {
  if (workflow?.definition_id === "github_issue_pr" && workflow.id) return workflow.id;
  return null;
}

function rejectedDecisions(decisions: WorkflowRuntimeDecisionRecord[]) {
  return decisions.filter((decision) => !decision.accepted);
}

function workflowRuntimeCounts(nodes: WorkflowRuntimeTreeNode[]) {
  let workflows = 0;
  let commands = 0;
  let jobs = 0;
  let rejected = 0;

  const visit = (node: WorkflowRuntimeTreeNode) => {
    workflows += 1;
    commands += node.commands.length;
    rejected += rejectedDecisions(node.decisions).length;
    for (const command of node.commands) jobs += command.runtime_jobs.length;
    for (const child of node.children) visit(child);
  };
  for (const node of nodes) visit(node);
  return { workflows, commands, jobs, rejected };
}

function WorkflowRuntimeNode({ node, depth = 0 }: { node: WorkflowRuntimeTreeNode; depth?: number }) {
  const rejected = rejectedDecisions(node.decisions);
  return (
    <div className="border-t border-line first:border-t-0 py-2">
      <div
        className="grid grid-cols-[minmax(120px,1fr)_auto_auto] items-start gap-2"
        style={{ paddingLeft: `${depth * 14}px` }}
      >
        <div className="min-w-0">
          <div className="truncate font-mono text-[11px] text-ink" title={node.workflow.id}>
            {node.workflow.definition_id} - {node.workflow.subject.subject_key}
          </div>
          <div className="mt-0.5 truncate font-mono text-[10px] text-ink-3">
            {node.events.length} events - {node.commands.length} commands
          </div>
        </div>
        <span className="border border-line bg-bg px-1.5 py-[1px] font-mono text-[10px] text-ink-2">
          {workflowLabel(node.workflow.state)}
        </span>
        {rejected.length > 0 ? (
          <span className="border border-rust/40 bg-rust/10 px-1.5 py-[1px] font-mono text-[10px] text-rust">
            rejected {rejected.length}
          </span>
        ) : null}
      </div>
      {node.commands.length > 0 ? (
        <div className="mt-1.5 space-y-1" style={{ marginLeft: `${depth * 14 + 10}px` }}>
          {node.commands.map((command) => (
            <div
              key={command.id}
              className="grid grid-cols-[minmax(110px,1fr)_auto] gap-2 font-mono text-[10px] text-ink-3"
            >
              <span className="truncate" title={command.command.dedupe_key}>
                activity: {commandLabel(command)}
              </span>
              <span className="truncate text-right" title={runtimeJobLabel(command)}>
                {runtimeJobLabel(command)}
              </span>
            </div>
          ))}
        </div>
      ) : null}
      {rejected.length > 0 ? (
        <div
          className="mt-1.5 truncate font-mono text-[10px] text-rust"
          style={{ marginLeft: `${depth * 14 + 10}px` }}
          title={rejected[0].rejection_reason ?? rejected[0].decision.reason}
        >
          rejected: {rejected[0].rejection_reason ?? rejected[0].decision.reason}
        </div>
      ) : null}
      {node.children.length > 0 ? (
        <div className="mt-2">
          {node.children.map((child) => (
            <WorkflowRuntimeNode key={child.workflow.id} node={child} depth={depth + 1} />
          ))}
        </div>
      ) : null}
    </div>
  );
}

function WorkflowRuntimePanel({
  payload,
  isLoading,
  isError,
}: {
  payload?: WorkflowRuntimeTreePayload;
  isLoading: boolean;
  isError: boolean;
}) {
  const workflows = payload?.workflows ?? [];
  const counts = workflowRuntimeCounts(workflows);
  const empty = workflows.length === 0;
  return (
    <section className="border border-line bg-bg-1">
      <div className="border-b border-line px-3 py-2">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3">
            Workflow Runtime
          </div>
          <div className="flex flex-wrap items-center gap-2 font-mono text-[10px] text-ink-3">
            <span>{payload?.total_workflows ?? counts.workflows} workflows</span>
            <span>{counts.commands} commands</span>
            <span>{counts.jobs} jobs</span>
            <span>{counts.rejected} rejected</span>
          </div>
        </div>
      </div>
      <div className="max-h-[280px] overflow-auto px-3">
        {isLoading ? (
          <div className="py-3 font-mono text-[11px] text-ink-4">loading…</div>
        ) : isError ? (
          <div className="py-3 font-mono text-[11px] text-ink-4">workflow runtime unavailable</div>
        ) : empty ? (
          <div className="py-3 font-mono text-[11px] text-ink-4">—</div>
        ) : (
          workflows.map((node) => <WorkflowRuntimeNode key={node.workflow.id} node={node} />)
        )}
      </div>
    </section>
  );
}

function TaskCard({
  task,
  workflow,
  onClick,
  onMerge,
  merging,
}: {
  task: Task;
  workflow?: WorkflowSummary | null;
  onClick: () => void;
  onMerge?: (taskId: string, workflow?: WorkflowSummary | null) => void;
  merging?: boolean;
}) {
  const title = task.description?.trim() || task.repo || task.id.slice(0, 8);
  return (
    <div
      className="w-full text-left border border-line bg-bg px-2.5 py-2 mb-2 last:mb-0 hover:border-line-3 transition-colors cursor-pointer"
    >
      <button type="button" className="block w-full text-left" onClick={onClick}>
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
            {workflow.review_fallback ? (
              <span className="border border-line-3 bg-bg px-1.5 py-[1px] font-mono text-[10px] text-ink">
                {fallbackTierLabel(workflow.review_fallback.tier)}
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
        {workflow?.review_fallback && (
          <div
            className="mt-1 block font-mono text-[10px] text-ink-3 truncate"
            title={fallbackTriggerLabel(workflow.review_fallback.trigger) ?? undefined}
          >
            fallback: {fallbackTriggerLabel(workflow.review_fallback.trigger)}
          </div>
        )}
      </button>
      {task.pr_url && (
        <a
          href={task.pr_url}
          target="_blank"
          rel="noreferrer"
          className="mt-1 block font-mono text-[10px] text-rust hover:underline truncate"
        >
          {task.pr_url.replace(/^https:\/\/github\.com\//, "")}
        </a>
      )}
      {workflow?.state === "ready_to_merge" && onMerge && (
        <button
          type="button"
          disabled={merging}
          onClick={(e) => {
            e.stopPropagation();
            onMerge(task.id, workflow);
          }}
          className="mt-2 w-full border border-line bg-bg-1 px-2 py-1 font-mono text-[10px] text-ink-2 hover:border-line-3 hover:text-ink transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {merging ? "Merging…" : "Merge"}
        </button>
      )}
    </div>
  );
}

interface Props {
  projectFilter?: string | null;
}

export function Active({ projectFilter }: Props) {
  const [selectedTaskId, setSelectedTaskId] = useState<string | null>(null);
  const [merging, setMerging] = useState<Set<string>>(new Set());
  const { data, isLoading, isError } = useTasks();
  const { data: dashboard } = useDashboard();
  const queryClient = useQueryClient();

  const handleMerge = async (taskId: string, workflow?: WorkflowSummary | null) => {
    setMerging((prev) => new Set(prev).add(taskId));
    try {
      const runtimeWorkflowId = runtimeMergeWorkflowId(workflow);
      if (runtimeWorkflowId) {
        await apiFetch("/api/workflows/runtime/merge", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ workflow_id: runtimeWorkflowId }),
        });
      } else {
        await apiFetch(`/tasks/${taskId}/merge`, { method: "POST" });
      }
      await queryClient.invalidateQueries({ queryKey: ["tasks"] });
      await queryClient.invalidateQueries({ queryKey: ["workflow-runtime-tree"] });
    } finally {
      setMerging((prev) => {
        const next = new Set(prev);
        next.delete(taskId);
        return next;
      });
    }
  };

  const resolvedRoot = projectFilter
    ? (dashboard?.projects.find((p) => p.id === projectFilter)?.root ?? projectFilter)
    : null;
  const workflowRuntime = useWorkflowRuntimeTree(resolvedRoot);

  const active = (data ?? [])
    .filter(shouldShowTask)
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
    <div className="space-y-3">
      <WorkflowRuntimePanel
        payload={workflowRuntime.data}
        isLoading={workflowRuntime.isLoading}
        isError={workflowRuntime.isError}
      />
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
                  <TaskCard
                    key={t.id}
                    task={t}
                    workflow={t.workflow ?? null}
                    onClick={() => setSelectedTaskId(t.id)}
                    onMerge={handleMerge}
                    merging={merging.has(t.id)}
                  />
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
                <TaskCard
                  key={t.id}
                  task={t}
                  workflow={t.workflow ?? null}
                  onClick={() => setSelectedTaskId(t.id)}
                />
              ))}
            </div>
          </div>
        )}
      </div>
      <TaskDetailSlideover
        taskId={selectedTaskId}
        onClose={() => setSelectedTaskId(null)}
      />
    </div>
  );
}
