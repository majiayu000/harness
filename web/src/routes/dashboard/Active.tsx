import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useAllTasks, useDashboard, useWorkflowRuntimeTree } from "@/lib/queries";
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
    workflowStates: ["discovered", "scheduled", "awaiting_dependencies"],
    fallbackTaskStatuses: ["pending", "queued", "awaiting_deps"],
  },
  {
    key: "planning",
    label: "Planning",
    workflowStates: ["planning", "replanning"],
    fallbackTaskStatuses: ["planning", "plan", "planner_generating", "planner_waiting"],
  },
  {
    key: "implementing",
    label: "Implementing",
    workflowStates: ["implementing"],
    fallbackTaskStatuses: ["implementing", "running", "triaging", "triage"],
  },
  {
    key: "review",
    label: "Review",
    workflowStates: ["local_review_gate"],
    fallbackTaskStatuses: ["review_generating", "review_waiting"],
  },
  {
    key: "feedback",
    label: "Feedback",
    workflowStates: ["pr_open", "awaiting_feedback", "addressing_feedback"],
    fallbackTaskStatuses: ["agent_review", "reviewing_agent", "waiting", "reviewing"],
  },
  {
    key: "validation",
    label: "Validation",
    workflowStates: ["quality_gate_pending"],
    fallbackTaskStatuses: [],
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

const TASK_CATEGORIES = [["all", "All work"], ["issue", "Issue fixes"], ["pr", "PR reviews"], ["periodic", "Scheduled reviews"], ["other", "Other tasks"]];

function taskCategory(task: Task): string {
  if (task.source === "periodic_review") return "periodic";
  if (task.id.startsWith("github-pr-feedback::") || task.task_kind === "review") return "pr";
  if (task.task_kind === "issue" || task.workflow?.definition_id === "github_issue_pr") return "issue";
  return "other";
}

const STAGE_HELP: Record<string, string> = {
  pending: "Waiting to start or waiting for dependencies.",
  implementing: "Implementation stage; check each task's execution status.",
  planning: "Preparing or revising an implementation plan.",
  review: "Repository scans and local code reviews; tasks may still be queued.",
  feedback: "Waiting for PR feedback or working on requested changes.",
  validation: "Checking tests and merge requirements.",
  ready: "Ready for the required merge approval.",
  blocked: "Requires a decision, dependency or error resolution.",
};

function executionLabel(task: Task, node?: WorkflowRuntimeTreeNode): string {
  const state = task.workflow?.state;
  if (["blocked", "degraded", "paused"].includes(state ?? "")) return "Blocked · open details for the reason";
  if (state === "awaiting_dependencies") return "Waiting for dependencies";
  if (state === "awaiting_feedback") return "Waiting for PR feedback";
  if (state === "ready_to_merge") return "Waiting for merge approval";
  const jobs = node?.commands.flatMap((command) => command.runtime_jobs) ?? [];
  const latest = jobs.reduce<WorkflowRuntimeJob | undefined>((a, b) =>
    !a || timestampValue(b.created_at) > timestampValue(a.created_at) ? b : a, undefined);
  if (latest?.status === "running") {
    if (latest.lease_state !== "active_leased") return "Worker lease unconfirmed";
    return latest.in_flight_model_turn ? "Model active · reported by runtime" : "Worker assigned · model activity unconfirmed";
  }
  if (latest?.status === "pending") return "Queued · waiting for a worker";
  if (task.scheduler.authority_state === "queued") return "Queued by scheduler";
  return "Execution unconfirmed · open details";
}

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

function commandLabel(command: WorkflowRuntimeCommandNode): string {
  const payload = command.command.command;
  const activity =
    payload && typeof payload === "object" && !Array.isArray(payload)
      ? (payload as Record<string, unknown>).activity
      : null;
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

function runtimeLeaseLabel(value?: string | null): string | null {
  if (!value) return null;
  return value.replace(/_/g, " ");
}

function compactRuntimeTimestamp(value?: string | null): string | null {
  if (!value) return null;
  return value.replace("T", " ").replace(/\.\d+Z$/, "Z");
}

function latestRuntimeJob(command: WorkflowRuntimeCommandNode): WorkflowRuntimeJob | null {
  return command.runtime_jobs.reduce<WorkflowRuntimeJob | null>((latest, job) => {
    if (!latest) return job;
    return runtimeJobUpdatedAt(job) >= runtimeJobUpdatedAt(latest) ? job : latest;
  }, null);
}

function runtimeJobLabel(command: WorkflowRuntimeCommandNode): string {
  const job = latestRuntimeJob(command);
  const totalJobs = command.runtime_job_count ?? command.runtime_jobs.length;
  const jobCountLabel =
    totalJobs === command.runtime_jobs.length
      ? `${totalJobs} jobs`
      : `${command.runtime_jobs.length}/${totalJobs} jobs`;
  if (!job) return jobCountLabel;
  const notBefore = job.not_before ? ` - not before ${job.not_before}` : "";
  const leaseState = runtimeLeaseLabel(job.lease_state);
  const lease = leaseState ? ` - ${leaseState}` : "";
  const inFlight = job.in_flight_model_turn ? " - in-flight" : "";
  const observedAt = compactRuntimeTimestamp(job.last_runtime_observation_at);
  const observed = observedAt ? ` - observed ${observedAt}` : "";
  const latestEvent = job.latest_runtime_event_type ? ` - event ${job.latest_runtime_event_type}` : "";
  const promptDigest = job.prompt_packet_digest ? ` - prompt ${job.prompt_packet_digest.slice(0, 12)}` : "";
  return `${jobCountLabel} - ${job.status}${lease}${inFlight}${observed}${notBefore}${latestEvent}${promptDigest}`;
}

function runtimeMergeWorkflowId(workflow?: WorkflowSummary | null): string | null {
  if (workflow?.definition_id === "github_issue_pr" && workflow.id) return workflow.id;
  return null;
}

function taskSubmissionHandle(task: Task): string {
  const submissionId = task.submission_id?.trim();
  return submissionId || task.id;
}

function runtimeWorkflowCanCancel(workflow: WorkflowRuntimeTreeNode["workflow"]): boolean {
  return (
    (workflow.definition_id === "github_issue_pr" || workflow.definition_id === "prompt_task") &&
    !TERMINAL_STATUSES.has(workflow.state)
  );
}

function rejectedDecisions(decisions: WorkflowRuntimeDecisionRecord[]) {
  return decisions.filter((decision) => !decision.accepted);
}

function rejectedDecisionCount(node: WorkflowRuntimeTreeNode) {
  return node.rejected_decision_count ?? rejectedDecisions(node.decisions).length;
}

function workflowRuntimeCounts(nodes: WorkflowRuntimeTreeNode[]) {
  let workflows = 0;
  let commands = 0;
  let jobs = 0;
  let rejected = 0;

  const visit = (node: WorkflowRuntimeTreeNode) => {
    workflows += 1;
    commands += node.command_count ?? node.commands.length;
    rejected += rejectedDecisionCount(node);
    jobs +=
      node.runtime_job_count ??
      node.commands.reduce((total, command) => total + command.runtime_jobs.length, 0);
    for (const child of node.children) visit(child);
  };
  for (const node of nodes) visit(node);
  return { workflows, commands, jobs, rejected };
}

function workflowRuntimePanelCounts(payload?: WorkflowRuntimeTreePayload) {
  const counts = workflowRuntimeCounts(payload?.workflows ?? []);
  const leaseStatuses = payload?.summary?.running_job_lease_statuses ?? {};
  return {
    ...counts,
    workflows: payload?.total_workflows ?? counts.workflows,
    commands: payload?.summary?.total_commands ?? counts.commands,
    jobs: payload?.summary?.total_runtime_jobs ?? counts.jobs,
    activeLeased: leaseStatuses.active_leased ?? 0,
    expiredOrMissing:
      (leaseStatuses.expired_lease ?? 0) + (leaseStatuses.missing_lease ?? 0),
  };
}

function workflowRuntimeWorkflowLabel(payload: WorkflowRuntimeTreePayload | undefined, fallback: number) {
  if (payload?.pagination) {
    return `${payload.pagination.returned}/${payload.pagination.total}`;
  }
  return `${payload?.total_workflows ?? fallback}`;
}

function WorkflowRuntimeNode({
  node,
  depth = 0,
  onCancel,
  cancellingWorkflowIds,
}: {
  node: WorkflowRuntimeTreeNode;
  depth?: number;
  onCancel?: (workflowId: string) => void;
  cancellingWorkflowIds?: Set<string>;
}) {
  const rejected = rejectedDecisions(node.decisions);
  const rejectedCount = rejectedDecisionCount(node);
  const canCancel = runtimeWorkflowCanCancel(node.workflow);
  const cancelling = cancellingWorkflowIds?.has(node.workflow.id) ?? false;
  return (
    <div className="border-t border-line first:border-t-0 py-2">
      <div
        className="grid grid-cols-[minmax(120px,1fr)_auto_auto_auto] items-start gap-2"
        style={{ paddingLeft: `${depth * 14}px` }}
      >
        <div className="min-w-0">
          <div className="truncate font-mono text-[11px] text-ink" title={node.workflow.id}>
            {node.workflow.definition_id} - {node.workflow.subject.subject_key}
          </div>
          <div className="mt-0.5 truncate font-mono text-[10px] text-ink-3">
            {node.event_count ?? node.events.length} events -{" "}
            {node.command_count ?? node.commands.length} commands
          </div>
        </div>
        <span className="border border-line bg-bg px-1.5 py-[1px] font-mono text-[10px] text-ink-2">
          {workflowLabel(node.workflow.state)}
        </span>
        {rejectedCount > 0 ? (
          <span className="border border-rust/40 bg-rust/10 px-1.5 py-[1px] font-mono text-[10px] text-rust">
            rejected {rejectedCount}
          </span>
        ) : null}
        {canCancel && onCancel ? (
          <button
            type="button"
            disabled={cancelling}
            onClick={(event) => {
              event.stopPropagation();
              onCancel(node.workflow.id);
            }}
            className="border border-line bg-bg px-1.5 py-[1px] font-mono text-[10px] text-ink-2 hover:border-rust/60 hover:text-rust transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {cancelling ? "Cancelling..." : "Cancel"}
          </button>
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
            <WorkflowRuntimeNode
              key={child.workflow.id}
              node={child}
              depth={depth + 1}
              onCancel={onCancel}
              cancellingWorkflowIds={cancellingWorkflowIds}
            />
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
  onCancel,
  cancellingWorkflowIds,
}: {
  payload?: WorkflowRuntimeTreePayload;
  isLoading: boolean;
  isError: boolean;
  onCancel?: (workflowId: string) => void;
  cancellingWorkflowIds?: Set<string>;
}) {
  const workflows = payload?.workflows ?? [];
  const counts = workflowRuntimePanelCounts(payload);
  const empty = workflows.length === 0;
  return (
    <section className="border border-line bg-bg-1">
      <div className="border-b border-line px-3 py-2">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3">
            Workflow Runtime
          </div>
          <div className="flex flex-wrap items-center gap-2 font-mono text-[10px] text-ink-3">
            <span>{workflowRuntimeWorkflowLabel(payload, counts.workflows)} workflows</span>
            <span>{counts.commands} commands</span>
            <span>{counts.jobs} jobs</span>
            <span>{counts.activeLeased} active leases</span>
            <span>{counts.expiredOrMissing} expired/missing</span>
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
          workflows.map((node) => (
            <WorkflowRuntimeNode
              key={node.workflow.id}
              node={node}
              onCancel={onCancel}
              cancellingWorkflowIds={cancellingWorkflowIds}
            />
          ))
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
  runtimeNode,
}: {
  task: Task;
  workflow?: WorkflowSummary | null;
  onClick: () => void;
  onMerge?: (taskId: string, workflow?: WorkflowSummary | null) => void;
  merging?: boolean;
  runtimeNode?: WorkflowRuntimeTreeNode;
}) {
  const handle = taskSubmissionHandle(task);
  const repo = task.repo || (task.pr_url?.match(/github\.com\/([^/]+\/[^/]+)/)?.[1])
    || task.project?.split("/").filter(Boolean).at(-1) || "Unknown repository";
  const title = task.description?.trim() && task.description !== "prompt task"
    ? task.description : taskCategory(task) === "periodic" ? "Scheduled repository review"
    : taskCategory(task) === "pr" ? "Pull request review" : "Repository task";
  const execution = executionLabel(task, runtimeNode);
  const observed = runtimeNode?.commands.flatMap((command) => command.runtime_jobs)
    .map((job) => job.last_runtime_observation_at)
    .filter((value): value is string => Boolean(value))
    .sort((a, b) => timestampValue(b) - timestampValue(a))[0];
  const updated = observed || runtimeNode?.workflow.updated_at || task.updated_at;
  return (
    <div
      className="w-full text-left border border-line bg-bg px-2.5 py-2 mb-2 last:mb-0 hover:border-line-3 transition-colors cursor-pointer"
    >
      <button type="button" className="block w-full text-left" onClick={onClick}>
        <div className="mb-1 truncate text-sm font-semibold text-ink" title={task.project ?? repo}>{repo}</div>
        <div className="text-[12.5px] text-ink leading-snug line-clamp-2" title={title}>
          {title}
        </div>
        {workflow && (
          <div className="mt-1 flex flex-wrap items-center gap-1">
            <span className="border border-line bg-bg-1 px-1.5 py-[1px] font-mono text-[10px] text-ink-2">
              wf {task.source === "periodic_review" && workflow.state === "implementing" ? "Repository Review" : workflowLabel(workflow.state)}
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
          <span className="truncate">{taskCategory(task) === "periodic" ? "Scheduled scan" : task.source || "Task"}</span>
          {task.turn > 0 && <span>turn {task.turn}</span>}
        </div>
        <div className="mt-2 border-t border-line pt-2 text-[11px] text-ink-2">{execution}</div>
        <div className="mt-1 text-[10px] text-ink-3">
          {updated ? <>Updated <time dateTime={updated}>{new Date(updated).toLocaleString()}</time></> : "Update time unavailable"}
        </div>
        {task.error && <p className="mt-2 text-xs text-rust break-words">{task.error}</p>}
        {workflow?.plan_concern && (
          <div
            className="mt-1 block font-mono text-[10px] text-rust truncate"
            title={workflow.plan_concern}
          >
            concern: {workflow.plan_concern}
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
      {workflow?.state === "ready_to_merge" && runtimeMergeWorkflowId(workflow) && onMerge && (
        <button
          type="button"
          disabled={merging}
          onClick={(e) => {
            e.stopPropagation();
            onMerge(handle, workflow);
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
  const [category, setCategory] = useState("all");
  const [search, setSearch] = useState("");
  const [selectedTaskId, setSelectedTaskId] = useState<string | null>(null);
  const [merging, setMerging] = useState<Set<string>>(new Set());
  const [mergeError, setMergeError] = useState<string | null>(null);
  const [cancellingWorkflows, setCancellingWorkflows] = useState<Set<string>>(new Set());
  const { data: dashboard } = useDashboard();
  const queryClient = useQueryClient();

  const handleMerge = async (taskId: string, workflow?: WorkflowSummary | null) => {
    setMergeError(null);
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
        throw new Error("Workflow runtime id unavailable; merge was not sent");
      }
      await queryClient.invalidateQueries({ queryKey: ["tasks"] });
      await queryClient.invalidateQueries({ queryKey: ["workflow-runtime-tree"] });
    } catch (error) {
      setMergeError(error instanceof Error ? error.message : "Merge failed");
    } finally {
      setMerging((prev) => {
        const next = new Set(prev);
        next.delete(taskId);
        return next;
      });
    }
  };

  const handleCancelWorkflow = async (workflowId: string) => {
    setCancellingWorkflows((prev) => new Set(prev).add(workflowId));
    try {
      await apiFetch("/api/workflows/runtime/cancel", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ workflow_id: workflowId }),
      });
    } catch (error) {
      console.error("Failed to cancel runtime workflow", error);
    } finally {
      const refreshResults = await Promise.allSettled([
        queryClient.invalidateQueries({ queryKey: ["tasks"] }),
        queryClient.invalidateQueries({ queryKey: ["workflow-runtime-tree"] }),
      ]);
      for (const result of refreshResults) {
        if (result.status === "rejected") {
          console.error(
            "Failed to refresh dashboard after runtime workflow cancellation",
            result.reason,
          );
        }
      }
      setCancellingWorkflows((prev) => {
        const next = new Set(prev);
        next.delete(workflowId);
        return next;
      });
    }
  };

  const resolvedRoot = projectFilter
    ? (dashboard?.projects.find((p) => p.id === projectFilter)?.root ?? projectFilter)
    : null;
  const { data, isLoading, isError } = useAllTasks({
    active: true,
    limit: 200,
    project_id: resolvedRoot ?? undefined,
  });
  const workflowRuntime = useWorkflowRuntimeTree(resolvedRoot);

  const allActive = (data?.data ?? []).filter(shouldShowTask);
  const runtimeNodes = new Map<string, WorkflowRuntimeTreeNode>();
  const visit = (node: WorkflowRuntimeTreeNode) => {
    runtimeNodes.set(node.workflow.id, node);
    node.children.forEach(visit);
  };
  workflowRuntime.data?.workflows.forEach(visit);
  const active = allActive.filter((task) =>
    (category === "all" || taskCategory(task) === category) &&
    [task.description, task.repo, task.project, task.pr_url, task.id].some(
      (value) => value?.toLowerCase().includes(search.toLowerCase()),
    ),
  );
  const grouped: Record<string, Task[]> = {};
  for (const c of COLUMNS) grouped[c.key] = [];
  const other: Task[] = [];
  for (const t of active) {
    const workflow = t.workflow ?? null;
    const col = t.source === "periodic_review" && workflow?.state === "implementing"
      ? "review" : columnOf(t.status, workflow?.state ?? null);
    if (col === "other") other.push(t);
    else grouped[col].push(t);
  }
  const showOther = other.length > 0;

  return (
    <div className="space-y-3">
      <header className="border-b border-line pb-4">
        <h1 className="text-2xl text-ink">Work in progress</h1>
        <p className="mt-2 text-sm text-ink-3">Choose a work type, then open a task for its logs and results. Stages do not indicate live agent processes.</p>
        <div className="mt-4 flex flex-wrap gap-6 text-sm text-ink-2" aria-label="Task summary">
          <span><strong className="text-xl text-ink">{isError ? "Unavailable" : isLoading ? "…" : allActive.length}</strong> unfinished tasks</span>
          <span><strong className="text-xl text-rust">{isError ? "Unavailable" : allActive.filter((t) => ["blocked", "degraded", "paused"].includes(t.workflow?.state ?? "")).length}</strong> need attention</span>
          <a className="text-rust underline" href="?tab=history">Completed tasks and review results →</a>
        </div>
      </header>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap gap-2" aria-label="Work type">
          {TASK_CATEGORIES.map(([key, label]) => (
            <button key={key} type="button" aria-pressed={category === key} onClick={() => setCategory(key)}
              className={`border px-3 py-2 text-xs ${category === key ? "border-ink bg-ink text-bg" : "border-line text-ink-2 hover:border-line-3"}`}>
              {label} <span className="ml-2 font-mono">{allActive.filter((t) => key === "all" || taskCategory(t) === key).length}</span>
            </button>
          ))}
        </div>
        <input aria-label="Filter tasks" placeholder="Filter by repository or task…" value={search} onChange={(e) => setSearch(e.target.value)}
          className="min-w-0 w-full sm:w-72 border border-line bg-bg px-3 py-2 text-sm text-ink" />
      </div>
      {isError && <p role="alert" className="text-rust">Task data unavailable. Counts and stages cannot be confirmed.</p>}
      {isLoading && <p role="status" className="text-ink-3">Loading tasks…</p>}
      {!isLoading && !isError && active.length === 0 && <p className="text-sm text-ink-3">No unfinished tasks match this view.</p>}
      <details className="border border-line p-3 text-xs text-ink-3">
        <summary className="cursor-pointer">Scheduler diagnostics · jobs, leases and workflow events</summary>
        <p className="my-3">Worker leases are not live Cursor processes. This diagnostic view may contain only part of the workflow history.</p>
        <WorkflowRuntimePanel
        payload={workflowRuntime.data}
        isLoading={workflowRuntime.isLoading}
        isError={workflowRuntime.isError}
        onCancel={handleCancelWorkflow}
        cancellingWorkflowIds={cancellingWorkflows}
      />
      </details>
      {mergeError ? (
        <div role="alert" className="border border-rust/40 bg-rust/10 px-3 py-2 text-xs text-rust">
          {mergeError}
        </div>
      ) : null}
      <div
        className="grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3"
      >
        {COLUMNS.map((col) => {
          const rows = grouped[col.key];
          return (
            <div key={col.key} className="border border-line bg-bg-1 min-h-[200px] flex flex-col">
              <div className="px-3 py-2 border-b border-line font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3 flex justify-between flex-none">
                <span>{col.label}</span>
                <span className="text-ink-2">{rows.length}</span>
              </div>
              <p className="px-3 pt-2 text-[11px] text-ink-3">{STAGE_HELP[col.key]}</p>
              <div className="p-2 flex-1 max-h-[480px] overflow-auto">
                {rows.length === 0 && (
                  <div className="text-ink-4 font-mono text-[11px] p-1">
                    {isLoading ? "loading…" : isError ? "error" : "—"}
                  </div>
                )}
                {rows.map((t) => (
                  <TaskCard
                    key={t.id}
                    task={t}
                    runtimeNode={runtimeNodes.get(t.workflow?.id ?? "")}
                    workflow={t.workflow ?? null}
                    onClick={() => setSelectedTaskId(taskSubmissionHandle(t))}
                    onMerge={handleMerge}
                    merging={merging.has(taskSubmissionHandle(t))}
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
                  runtimeNode={runtimeNodes.get(t.workflow?.id ?? "")}
                  workflow={t.workflow ?? null}
                  onClick={() => setSelectedTaskId(taskSubmissionHandle(t))}
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
