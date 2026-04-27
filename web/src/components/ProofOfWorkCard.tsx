import { workflowLabel } from "@/lib/format";
import type { FullTask, TaskArtifact, TaskPrompt } from "@/types";

interface Props {
  task: FullTask;
  prompts: TaskPrompt[] | undefined;
  artifacts: TaskArtifact[] | undefined;
}

function reviewerVerdict(state: string): string {
  switch (state) {
    case "ready_to_merge": return "Approved";
    case "awaiting_feedback": return "Changes Requested";
    case "addressing_feedback": return "In Progress";
    default: return workflowLabel(state);
  }
}

function elapsedTime(createdAt: string | null, completedAt: string | undefined): string {
  if (!createdAt || !completedAt) return "N/A";
  const ms = new Date(completedAt).getTime() - new Date(createdAt).getTime();
  if (isNaN(ms) || ms < 0) return "N/A";
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

export function ProofOfWorkCard({ task, prompts, artifacts }: Props) {
  return (
    <div
      className="mt-4 border border-line bg-bg p-3 flex flex-col gap-3 font-mono text-[11px]"
      data-testid="proof-of-work-card"
    >
      <section>
        <div className="text-ink-3 mb-1 uppercase tracking-[0.08em]">PR &amp; GitHub State</div>
        {task.pr_url ? (
          <div className="flex flex-col gap-1">
            <a
              href={task.pr_url}
              target="_blank"
              rel="noreferrer"
              className="text-rust hover:underline break-all"
            >
              {task.pr_url}
            </a>
            {task.workflow?.state && (
              <span className="border border-line bg-bg-1 px-1.5 py-[1px] text-[10px] text-ink-2 self-start">
                {workflowLabel(task.workflow.state)}
              </span>
            )}
          </div>
        ) : (
          <span className="text-ink-3">No PR</span>
        )}
      </section>

      <section>
        <div className="text-ink-3 mb-1 uppercase tracking-[0.08em]">Rounds / Elapsed</div>
        <div className="flex gap-4 text-ink">
          <span>Rounds: {task.turn}</span>
          <span>Elapsed: {elapsedTime(task.created_at, task.completed_at)}</span>
        </div>
      </section>

      <section>
        <div className="text-ink-3 mb-1 uppercase tracking-[0.08em]">Reviewer Verdict</div>
        {task.workflow?.state ? (
          <span className="text-ink">{reviewerVerdict(task.workflow.state)}</span>
        ) : (
          <span className="text-ink-3">No reviewer data</span>
        )}
      </section>

      <section>
        <div className="text-ink-3 mb-1 uppercase tracking-[0.08em]">Prompts</div>
        {prompts === undefined ? (
          <span className="text-ink-3">Loading prompts…</span>
        ) : prompts.length === 0 ? (
          <span className="text-ink-3">No prompts recorded</span>
        ) : (
          <div className="flex flex-col gap-1">
            {prompts.map((p, i) => (
              <details key={i} className="border border-line">
                <summary className="px-2 py-1 cursor-pointer text-ink-2 hover:bg-bg-1">
                  {p.phase} (turn {p.turn})
                </summary>
                <pre className="p-2 text-ink whitespace-pre-wrap break-words text-[10px]">
                  {p.prompt.length > 500 ? p.prompt.slice(0, 500) + "…" : p.prompt}
                </pre>
              </details>
            ))}
          </div>
        )}
      </section>

      <section>
        <div className="text-ink-3 mb-1 uppercase tracking-[0.08em]">Artifacts</div>
        {artifacts === undefined ? (
          <span className="text-ink-3">Loading artifacts…</span>
        ) : artifacts.length === 0 ? (
          <span className="text-ink-3">No artifacts</span>
        ) : (
          <div className="flex flex-col gap-2">
            {artifacts.map((a, i) => (
              <div key={i} className="border border-line p-2">
                <span className="border border-line bg-bg-1 px-1.5 py-[1px] text-[10px] text-ink-2">
                  {a.artifact_type}
                </span>
                <pre className="mt-1 text-ink whitespace-pre-wrap break-words text-[10px]">
                  {a.content}
                </pre>
              </div>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}
