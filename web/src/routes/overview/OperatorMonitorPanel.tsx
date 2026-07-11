import { useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Panel } from "@/components/Panel";
import { apiFetch } from "@/lib/api";
import { useOperatorMonitor } from "@/lib/queries";
import { fmtInt } from "@/lib/format";
import type {
  FailureGroup,
  OperatorAction,
  OperatorMonitorPayload,
  RuntimeStoppedState,
  SourceActivity,
  StuckWorkflow,
} from "@/types";

type WorkflowRecoveryAction = "unblock" | "retry";

interface WorkflowRecoveryInput {
  action: WorkflowRecoveryAction;
  workflowId: string;
  reason: string;
}

function fmtDuration(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3_600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3_600)}h`;
  return `${Math.floor(seconds / 86_400)}d`;
}

function sourceTotal(source: SourceActivity): number {
  return source.pending + source.running + source.review + source.blocked + source.failed + source.ready_to_merge;
}

function Metric({ label, value, tone = "default" }: { label: string; value: string | number; tone?: "default" | "ok" | "warn" | "err" }) {
  const color = tone === "ok" ? "text-ok" : tone === "warn" ? "text-warn" : tone === "err" ? "text-danger" : "text-ink-1";
  return (
    <div className="min-w-0 border-r border-line px-4 py-3 last:border-r-0">
      <div className={`font-mono text-lg font-semibold ${color}`}>{value}</div>
      <div className="mt-0.5 truncate font-mono text-[10px] uppercase tracking-[0.1em] text-ink-4">{label}</div>
    </div>
  );
}

function StructuredStopDetails({
  state,
  stopped,
}: {
  state: string;
  stopped: RuntimeStoppedState;
}) {
  const lastStopState = stopped.last_stop?.state;
  const displayState =
    state === "blocked" || state === "failed"
      ? state
      : lastStopState === "blocked" || lastStopState === "failed"
        ? lastStopState
        : null;
  const reason =
    displayState === "blocked"
      ? stopped.blocked_reason?.trim()
      : displayState === "failed"
        ? stopped.failure_reason?.trim()
        : null;
  const hint =
    displayState === "blocked"
      ? stopped.unblock_hint?.trim()
      : displayState === "failed"
        ? stopped.retry_hint?.trim()
        : null;
  if (!reason && !hint) return null;

  return (
    <div className="mt-1 max-w-72 space-y-0.5 normal-case tracking-normal">
      {reason && <div className="line-clamp-2 text-[10px] text-ink-2">{reason}</div>}
      {hint && <div className="line-clamp-2 text-[10px] text-ink-4">{hint}</div>}
    </div>
  );
}

function RecoveryControl({
  workflowId,
  action,
  disabled,
  pending,
  onRecover,
}: {
  workflowId: string;
  action: WorkflowRecoveryAction;
  disabled: boolean;
  pending: boolean;
  onRecover: (input: WorkflowRecoveryInput) => void;
}) {
  const [reason, setReason] = useState("");
  const [validationError, setValidationError] = useState<string | null>(null);
  const label = action === "unblock" ? "Unblock" : "Retry";

  return (
    <form
      className="mt-2 flex min-w-52 flex-col items-end gap-1"
      onSubmit={(event) => {
        event.preventDefault();
        const trimmedReason = reason.trim();
        if (!trimmedReason) {
          setValidationError("Recovery reason is required.");
          return;
        }
        setValidationError(null);
        onRecover({ action, workflowId, reason: trimmedReason });
      }}
    >
      <div className="flex w-full gap-1">
        <input
          aria-label={`Recovery reason for ${workflowId}`}
          className="min-w-0 flex-1 border border-line bg-bg-1 px-2 py-1 text-[10px] text-ink-2 placeholder:text-ink-4 focus:border-rust focus:outline-none"
          disabled={disabled}
          onChange={(event) => setReason(event.target.value)}
          placeholder="Operator reason"
          type="text"
          value={reason}
        />
        <button
          aria-label={`${label} workflow ${workflowId}`}
          className="border border-rust/50 px-2 py-1 text-[10px] text-rust hover:bg-rust/5 disabled:cursor-not-allowed disabled:opacity-50"
          disabled={disabled}
          type="submit"
        >
          {pending ? `${label}ing…` : label}
        </button>
      </div>
      {validationError && (
        <span className="text-[10px] text-danger" role="alert">
          {validationError}
        </span>
      )}
    </form>
  );
}

function ActionRow({
  action,
  recoveryDisabled,
  recoveryPending,
  onRecover,
}: {
  action: OperatorAction;
  recoveryDisabled: boolean;
  recoveryPending: boolean;
  onRecover: (input: WorkflowRecoveryInput) => void;
}) {
  const recoveryAction: WorkflowRecoveryAction | null =
    action.can_unblock === true ? "unblock" : action.can_retry === true ? "retry" : null;

  return (
    <tr className="border-b border-line last:border-b-0">
      <td className="px-3 py-2 font-mono text-[11px] text-ink-2">{action.kind.replace(/_/g, " ")}</td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-3">{action.repo ?? "untracked"}</td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-3">
        {action.issue ? `#${action.issue}` : "unknown"}
        {action.pr ? ` -> PR #${action.pr}` : ""}
      </td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-2">
        <div>{action.state}</div>
        <StructuredStopDetails state={action.state} stopped={action} />
      </td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-4">{fmtDuration(action.age_secs)}</td>
      <td className="px-3 py-2 text-right font-mono text-[11px]">
        {action.url ? (
          <a href={action.url} target="_blank" rel="noreferrer" className="text-rust hover:underline">
            open
          </a>
        ) : action.evidence_url ? (
          <a href={action.evidence_url} className="text-rust hover:underline">
            evidence
          </a>
        ) : (
          <span className="text-ink-4">unlinked</span>
        )}
        {recoveryAction && (
          <RecoveryControl
            action={recoveryAction}
            disabled={recoveryDisabled}
            onRecover={onRecover}
            pending={recoveryPending}
            workflowId={action.workflow_id}
          />
        )}
      </td>
    </tr>
  );
}

function FailureRow({ failure }: { failure: FailureGroup }) {
  const tone = failure.severity === "warn" ? "text-warn" : failure.severity === "critical" ? "text-danger" : "text-rust";
  return (
    <tr className="border-b border-line last:border-b-0">
      <td className={`px-3 py-2 font-mono text-[11px] ${tone}`}>{failure.family}</td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-3">{failure.repo ?? "untracked"}</td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-2">
        <span className="line-clamp-1">{failure.message}</span>
      </td>
      <td className="px-3 py-2 text-right font-mono text-[11px] text-ink-3">{fmtInt(failure.count)}</td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-4">{failure.retryable ? "retryable" : "manual"}</td>
    </tr>
  );
}

function StuckWorkflowRow({ workflow }: { workflow: StuckWorkflow }) {
  return (
    <tr className="border-b border-line last:border-b-0">
      <td className="px-3 py-2 font-mono text-[11px] text-warn">
        <div>{workflow.state.replace(/_/g, " ")}</div>
        <StructuredStopDetails state={workflow.state} stopped={workflow} />
      </td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-3">{workflow.repo ?? "untracked"}</td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-3">
        {workflow.issue ? `#${workflow.issue}` : workflow.workflow_id}
        {workflow.pr ? ` -> PR #${workflow.pr}` : ""}
      </td>
      <td className="px-3 py-2 font-mono text-[11px] text-ink-4">{fmtDuration(workflow.age_secs)}</td>
      <td className="px-3 py-2 text-right font-mono text-[11px]">
        {workflow.url ? (
          <a href={workflow.url} target="_blank" rel="noreferrer" className="text-rust hover:underline">
            open
          </a>
        ) : (
          <span className="text-ink-4">unlinked</span>
        )}
      </td>
    </tr>
  );
}

function SourceRow({ source }: { source: SourceActivity }) {
  return (
    <tr className="border-b border-line last:border-b-0">
      <td className="px-3 py-2 font-mono text-[11px] text-ink-2">{source.source}</td>
      <td className="px-3 py-2 text-right font-mono text-[11px] text-ink-3">{fmtInt(source.pending)}</td>
      <td className="px-3 py-2 text-right font-mono text-[11px] text-ink-1">{fmtInt(source.running)}</td>
      <td className="px-3 py-2 text-right font-mono text-[11px] text-ink-2">{fmtInt(source.review)}</td>
      <td className="px-3 py-2 text-right font-mono text-[11px] text-sand">{fmtInt(source.ready_to_merge)}</td>
      <td className="px-3 py-2 text-right font-mono text-[11px] text-warn">{fmtInt(source.blocked)}</td>
      <td className="px-3 py-2 text-right font-mono text-[11px] text-danger">{fmtInt(source.failed)}</td>
      <td className="px-3 py-2 text-right font-mono text-[11px] text-ink-3">{fmtInt(sourceTotal(source))}</td>
    </tr>
  );
}

function MonitorBody({
  data,
  recoveryDisabled,
  recoveryPendingWorkflowId,
  onRecover,
}: {
  data: OperatorMonitorPayload;
  recoveryDisabled: boolean;
  recoveryPendingWorkflowId: string | null;
  onRecover: (input: WorkflowRecoveryInput) => void;
}) {
  const runtime = data.activity.runtime_workflows;
  const legacy = data.activity.legacy_queue;
  const healthTone = data.health.status === "ok" ? "ok" : "warn";
  const topWorktrees = data.worktrees.cards.slice(0, 4);

  return (
    <>
      <div className="grid grid-cols-8 border-b border-line">
        <Metric label="health" value={data.health.status} tone={healthTone} />
        <Metric label="runtime running" value={fmtInt(runtime.running)} />
        <Metric label="runtime review" value={fmtInt(runtime.review)} />
        <Metric label="ready" value={fmtInt(runtime.ready_to_merge)} tone={runtime.ready_to_merge > 0 ? "warn" : "default"} />
        <Metric label="blocked" value={fmtInt(runtime.blocked)} tone={runtime.blocked > 0 ? "err" : "default"} />
        <Metric label="legacy running" value={fmtInt(legacy.running)} />
        <Metric label="legacy queued" value={fmtInt(legacy.queued)} />
        <Metric label="worktrees" value={`${fmtInt(data.worktrees.used)}/${fmtInt(data.worktrees.capacity)}`} />
      </div>

      <div className="grid grid-cols-[1.2fr_1fr] border-b border-line">
        <div className="border-r border-line">
          <div className="px-4 py-2 font-mono text-[10px] uppercase tracking-[0.1em] text-ink-3">Operator actions</div>
          {data.operator_actions.length === 0 ? (
            <p className="border-t border-line px-4 py-3 font-mono text-[11px] text-ink-4">No current operator actions.</p>
          ) : (
            <table className="w-full border-t border-line">
              <tbody>
                {data.operator_actions.map((action) => (
                  <ActionRow
                    key={action.workflow_id}
                    action={action}
                    onRecover={onRecover}
                    recoveryDisabled={recoveryDisabled}
                    recoveryPending={recoveryPendingWorkflowId === action.workflow_id}
                  />
                ))}
              </tbody>
            </table>
          )}
        </div>

        <div>
          <div className="px-4 py-2 font-mono text-[10px] uppercase tracking-[0.1em] text-ink-3">Source split</div>
          {data.activity.by_source.length === 0 ? (
            <p className="border-t border-line px-4 py-3 font-mono text-[11px] text-ink-4">No active source rows.</p>
          ) : (
            <table className="w-full border-t border-line">
              <tbody>{data.activity.by_source.map((source) => <SourceRow key={source.source} source={source} />)}</tbody>
            </table>
          )}
        </div>
      </div>

      <div className="grid grid-cols-[1.2fr_1fr]">
        <div className="border-r border-line">
          <div className="border-b border-line">
            <div className="px-4 py-2 font-mono text-[10px] uppercase tracking-[0.1em] text-ink-3">Stuck workflows</div>
            {data.stuck_workflows.length === 0 ? (
              <p className="border-t border-line px-4 py-3 font-mono text-[11px] text-ink-4">No aged stuck workflows.</p>
            ) : (
              <table className="w-full border-t border-line">
                <tbody>{data.stuck_workflows.map((workflow) => <StuckWorkflowRow key={workflow.workflow_id} workflow={workflow} />)}</tbody>
              </table>
            )}
          </div>
          <div className="px-4 py-2 font-mono text-[10px] uppercase tracking-[0.1em] text-ink-3">Grouped failures</div>
          {data.failures.length === 0 ? (
            <p className="border-t border-line px-4 py-3 font-mono text-[11px] text-ink-4">No recent failures.</p>
          ) : (
            <table className="w-full border-t border-line">
              <tbody>{data.failures.map((failure) => <FailureRow key={`${failure.family}:${failure.repo}:${failure.message}`} failure={failure} />)}</tbody>
            </table>
          )}
        </div>

        <div>
          <div className="px-4 py-2 font-mono text-[10px] uppercase tracking-[0.1em] text-ink-3">Worktrees</div>
          <div className="border-t border-line px-4 py-3 font-mono text-[11px] text-ink-3">
            metrics {data.worktrees.metrics_state === "available" ? "available" : "unavailable"}
            {data.worktrees.stale != null ? ` · stale ${fmtInt(data.worktrees.stale)}` : " · stale unknown"}
          </div>
          {topWorktrees.length > 0 && (
            <div className="border-t border-line">
              {topWorktrees.map((card) => (
                <div key={card.task_id} className="grid grid-cols-[1fr_auto] gap-3 border-b border-line px-4 py-2 last:border-b-0">
                  <span className="truncate font-mono text-[11px] text-ink-2" title={card.workspace_path}>
                    {card.path_short}
                  </span>
                  <span className="font-mono text-[11px] text-ink-4">{card.status}</span>
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </>
  );
}

export function OperatorMonitorPanel() {
  const { data, isError } = useOperatorMonitor();
  const queryClient = useQueryClient();
  const recovery = useMutation<Response, Error, WorkflowRecoveryInput>({
    mutationFn: ({ action, workflowId, reason }) =>
      apiFetch(`/api/workflows/runtime/${action}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ workflow_id: workflowId, reason }),
      }),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["operator-monitor"] }),
        queryClient.invalidateQueries({ queryKey: ["workflow-runtime-tree"] }),
      ]);
    },
  });

  return (
    <Panel title="Operator monitor" sub="runtime · actions · failures · worktrees" id="operator-monitor">
      {recovery.error && (
        <p className="border-b border-line px-4 py-3 font-mono text-[11px] text-danger" role="alert">
          Workflow recovery failed: {recovery.error.message}
        </p>
      )}
      {isError ? (
        <p className="px-4 py-3 font-mono text-[11px] text-danger">Operator monitor unavailable.</p>
      ) : data ? (
        <MonitorBody
          data={data}
          onRecover={(input) => recovery.mutate(input)}
          recoveryDisabled={recovery.isPending}
          recoveryPendingWorkflowId={recovery.variables?.workflowId ?? null}
        />
      ) : (
        <p className="px-4 py-3 font-mono text-[11px] text-ink-4">Loading operator monitor.</p>
      )}
    </Panel>
  );
}
