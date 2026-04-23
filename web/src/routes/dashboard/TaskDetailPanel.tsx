import { useEffect, useState } from "react";
import { TOKEN_KEY } from "@/lib/api";
import { useTaskArtifacts, useTaskDetail, useTaskPrompts } from "@/lib/queries";

interface Props {
  taskId: string | null;
}

function buildStreamUrl(taskId: string): string {
  const tok = (globalThis.sessionStorage?.getItem?.(TOKEN_KEY) ?? "").trim();
  const base = `/tasks/${taskId}/stream`;
  return tok ? `${base}?token=${encodeURIComponent(tok)}` : base;
}

function titleForTask(taskId: string, description?: string | null, repo?: string | null): string {
  return description?.trim() || repo || taskId;
}

export function TaskDetailPanel({ taskId }: Props) {
  const detail = useTaskDetail(taskId);
  const artifactsQuery = useTaskArtifacts(taskId);
  const promptsQuery = useTaskPrompts(taskId);
  const [streamText, setStreamText] = useState("");

  const task = detail.data;
  const prompts = promptsQuery.data ?? task?.prompts ?? [];
  const artifacts = artifactsQuery.data ?? task?.artifacts ?? [];

  useEffect(() => {
    setStreamText("");
  }, [taskId]);

  useEffect(() => {
    if (!taskId || !task || task.completion.is_terminal || typeof EventSource !== "function") {
      return;
    }
    const source = new EventSource(buildStreamUrl(taskId));
    source.onmessage = (event) => {
      try {
        const item = JSON.parse(event.data) as { type?: string; text?: string; message?: string };
        if (item.type === "message_delta" && item.text) {
          setStreamText((current) => current + item.text);
        } else if (item.type === "error" && item.message) {
          setStreamText((current) => `${current}\n[error] ${item.message}`.trim());
        }
      } catch {
        setStreamText((current) => `${current}\n${event.data}`.trim());
      }
    };
    return () => source.close();
  }, [task, taskId]);

  if (!taskId) {
    return (
      <aside className="border border-line bg-bg-1 p-4 text-[12px] text-ink-3">
        Select a task to inspect its live output and completion evidence.
      </aside>
    );
  }

  if (detail.isLoading) {
    return <aside className="border border-line bg-bg-1 p-4 font-mono text-[12px] text-ink-3">Loading task…</aside>;
  }

  if (detail.error || !task) {
    return (
      <aside className="border border-line bg-bg-1 p-4 font-mono text-[12px] text-rust">
        {detail.error?.message ?? "Task not found"}
      </aside>
    );
  }

  const latestPrompt = prompts.at(-1) ?? null;
  const latestArtifact = artifacts.at(-1) ?? null;

  return (
    <aside className="border border-line bg-bg-1 min-h-[320px]">
      <div className="border-b border-line px-4 py-3">
        <div className="font-mono text-[10.5px] uppercase tracking-[0.1em] text-ink-3">
          Task detail
        </div>
        <h2 className="mt-2 text-[16px] leading-snug text-ink">
          {titleForTask(task.id, task.description, task.repo)}
        </h2>
        <div className="mt-2 flex flex-wrap gap-2 font-mono text-[11px] text-ink-3">
          <span>{task.status}</span>
          {task.workflow?.state ? <span>workflow {task.workflow.state}</span> : null}
          {task.project ? <span>{task.project}</span> : null}
        </div>
      </div>

      <div className="space-y-4 p-4">
        <section>
          <div className="font-mono text-[10.5px] uppercase tracking-[0.1em] text-ink-3">
            {task.completion.is_terminal ? "Completion" : "Live output"}
          </div>
          {task.completion.is_terminal ? (
            <div className="mt-2 grid gap-2 text-[12px] text-ink-2">
              <div>
                PR:{" "}
                {task.completion.pr_url ? (
                  <a href={task.completion.pr_url} target="_blank" rel="noreferrer" className="text-rust hover:underline">
                    {task.completion.pr_url}
                  </a>
                ) : (
                  <span>Not available yet</span>
                )}
              </div>
              <div>Prompts: {task.completion.has_prompts ? `${prompts.length} saved` : "Not available yet"}</div>
              <div>Artifacts: {task.completion.has_artifacts ? `${artifacts.length} saved` : "Not available yet"}</div>
              <div>
                Checkpoint: {task.completion.checkpoint?.last_phase ?? "Not available yet"}
              </div>
            </div>
          ) : (
            <pre className="mt-2 max-h-[320px] overflow-auto whitespace-pre-wrap border border-line-2 bg-bg px-3 py-2 text-[12px] text-ink">
              {streamText || "Waiting for live output…"}
            </pre>
          )}
        </section>

        <section>
          <div className="font-mono text-[10.5px] uppercase tracking-[0.1em] text-ink-3">Latest prompt</div>
          <div className="mt-2 border border-line-2 bg-bg px-3 py-2 text-[12px] text-ink-2 whitespace-pre-wrap">
            {latestPrompt?.prompt || "Not available yet"}
          </div>
        </section>

        <section>
          <div className="font-mono text-[10.5px] uppercase tracking-[0.1em] text-ink-3">Latest artifact</div>
          <div className="mt-2 border border-line-2 bg-bg px-3 py-2 text-[12px] text-ink-2 whitespace-pre-wrap">
            {latestArtifact?.content || "Not available yet"}
          </div>
        </section>

        <section>
          <div className="font-mono text-[10.5px] uppercase tracking-[0.1em] text-ink-3">Plan checkpoint</div>
          <div className="mt-2 border border-line-2 bg-bg px-3 py-2 text-[12px] text-ink-2 whitespace-pre-wrap">
            {task.completion.checkpoint?.plan_output ||
              task.completion.checkpoint?.triage_output ||
              "Not available yet"}
          </div>
        </section>
      </div>
    </aside>
  );
}
