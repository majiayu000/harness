import { useState } from "react";
import { useTaskStream, useCancelWorkflowRuntime } from "@/lib/queries";
import { TOKEN_KEY } from "@/lib/api";

interface Props {
  taskId: string;
  workflowId?: string | null;
  executionPath?: string | null;
  onReset: () => void;
}

export function SubmitSuccess({ taskId, workflowId, executionPath, onReset }: Props) {
  const [output, setOutput] = useState<string>("");
  const [streamError, setStreamError] = useState<string | null>(null);
  const cancelWorkflowRuntime = useCancelWorkflowRuntime();
  const canUseRuntimeSubmission = executionPath === "workflow_runtime";
  const canCancelWorkflowRuntime = executionPath === "workflow_runtime" && !!workflowId;
  const cancelPending = cancelWorkflowRuntime.isPending;

  useTaskStream(
    canUseRuntimeSubmission ? taskId : null,
    (text) => setOutput((prev) => prev + text),
    (err) => setStreamError(err),
  );

  function openStream() {
    const tok = (globalThis.sessionStorage?.getItem?.(TOKEN_KEY) ?? "").trim();
    const base = `/api/workflows/runtime/submissions/${taskId}/stream`;
    const url = tok ? `${base}?token=${encodeURIComponent(tok)}` : base;
    window.open(url, "_blank", "noreferrer");
  }

  function handleCancel() {
    if (canCancelWorkflowRuntime && workflowId) {
      cancelWorkflowRuntime.mutate(workflowId, { onSuccess: onReset });
    } else {
      setStreamError("Workflow runtime id unavailable; cancellation was not sent");
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <span className="font-mono text-[10.5px] tracking-[0.1em] uppercase text-ink-3">Task</span>
        <code className="font-mono text-[12px] text-ink px-1.5 py-0.5 bg-bg-2 border border-line-2 rounded-[3px]">
          {taskId}
        </code>
        <span className="font-mono text-[10.5px] px-1.5 py-[1px] border border-ok/40 text-ok rounded-[10px]">
          running
        </span>
      </div>
      <div className="flex gap-2">
        <button
          type="button"
          disabled={!canUseRuntimeSubmission}
          title={canUseRuntimeSubmission ? undefined : "Runtime submission unavailable"}
          onClick={openStream}
          className="font-mono text-[11.5px] px-3 py-1 border border-line-2 text-ink-2 rounded-[3px] hover:bg-bg-2 hover:text-ink disabled:opacity-50 disabled:cursor-not-allowed"
        >
          Watch live
        </button>
        <button
          type="button"
          disabled={cancelPending || !canCancelWorkflowRuntime}
          title={canCancelWorkflowRuntime ? undefined : "Workflow runtime id unavailable"}
          onClick={handleCancel}
          className="font-mono text-[11.5px] px-3 py-1 border border-danger/40 text-danger rounded-[3px] hover:bg-danger/5 disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {cancelPending ? "Cancelling..." : "Cancel"}
        </button>
        <button
          type="button"
          onClick={onReset}
          className="ml-auto font-mono text-[11.5px] px-3 py-1 border border-line-2 text-ink-3 rounded-[3px] hover:bg-bg-2"
        >
          Submit another
        </button>
      </div>
      {streamError && (
        <div
          data-testid="stream-error"
          className="px-3 py-2 border border-danger/40 text-danger font-mono text-[12px] rounded-[3px] bg-danger/5"
        >
          Stream error: {streamError}
        </div>
      )}
      {output && (
        <pre className="font-mono text-[11px] text-ink bg-bg border border-line p-4 overflow-auto max-h-[400px] rounded-[3px] whitespace-pre-wrap">
          {output}
        </pre>
      )}
    </div>
  );
}
