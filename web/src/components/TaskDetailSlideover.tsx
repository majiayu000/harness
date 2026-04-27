import { useEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useTaskDetail, useTaskStream } from "@/lib/queries";
import { apiJson } from "@/lib/api";
import { isTerminal } from "@/types/task";
import { ProofOfWorkCard } from "./ProofOfWorkCard";
import type { FullTask, TaskArtifact, TaskPrompt } from "@/types";

type Tab = "summary" | "output" | "prompts" | "artifacts";

const TABS: Tab[] = ["summary", "output", "prompts", "artifacts"];
const MAX_STREAM_CHARS = 50_000;

interface Props {
  taskId: string | null;
  onClose: () => void;
}

export function TaskDetailSlideover({ taskId, onClose }: Props) {
  const [activeTab, setActiveTab] = useState<Tab>("summary");
  const [streamText, setStreamText] = useState("");
  const [streamError, setStreamError] = useState<string | null>(null);
  const bodyRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    setStreamText("");
    setStreamError(null);
    setActiveTab("summary");
  }, [taskId]);

  useTaskStream(
    taskId,
    (text) =>
      setStreamText((prev) => {
        const next = prev + text;
        return next.length > MAX_STREAM_CHARS ? next.slice(-MAX_STREAM_CHARS) : next;
      }),
    (msg) => setStreamError(msg),
  );

  const { data: task, isLoading, isError } = useTaskDetail(taskId);

  const isTaskTerminal = task ? isTerminal(task.status) : false;

  const { data: artifacts, isError: isArtifactsError } = useQuery({
    queryKey: ["task-artifacts", taskId],
    queryFn: ({ signal }) => apiJson<TaskArtifact[]>(`/tasks/${taskId}/artifacts`, { signal }),
    enabled: !!taskId && (activeTab === "artifacts" || isTaskTerminal),
  });

  const { data: prompts, isError: isPromptsError } = useQuery({
    queryKey: ["task-prompts", taskId],
    queryFn: ({ signal }) => apiJson<TaskPrompt[]>(`/tasks/${taskId}/prompts`, { signal }),
    enabled: !!taskId && (activeTab === "prompts" || isTaskTerminal),
  });

  useEffect(() => {
    if (activeTab === "output" && bodyRef.current) {
      bodyRef.current.scrollTop = bodyRef.current.scrollHeight;
    }
  }, [streamText, activeTab]);

  useEffect(() => {
    if (!taskId) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [taskId, onClose]);

  if (!taskId) return null;

  return (
    <>
      <div
        className="fixed inset-0 z-[99] bg-black/40"
        onClick={onClose}
        aria-hidden="true"
        data-testid="slideover-scrim"
      />
      <div className="fixed right-0 top-0 h-full w-[480px] z-[100] bg-bg-1 border-l border-line-2 flex flex-col overflow-hidden">
        <div className="flex items-center justify-between px-4 py-3 border-b border-line flex-none">
          <div className="flex items-center gap-2 min-w-0">
            <span className="font-mono text-[11px] text-ink-3 truncate">
              {taskId.slice(0, 8)}
            </span>
            {task && (
              <span className="border border-line bg-bg px-1.5 py-[1px] font-mono text-[10px] text-ink-2">
                {task.status}
              </span>
            )}
          </div>
          <button
            onClick={onClose}
            className="text-ink-3 text-lg flex-none ml-2"
            aria-label="Close"
          >
            ×
          </button>
        </div>
        <div className="flex border-b border-line flex-none">
          {TABS.map((tab) => (
            <button
              key={tab}
              onClick={() => setActiveTab(tab)}
              className={`px-4 py-2 font-mono text-[11px] tracking-[0.08em] uppercase border-b-2 -mb-px ${
                activeTab === tab
                  ? "border-rust text-ink"
                  : "border-transparent text-ink-3 hover:text-ink-2"
              }`}
            >
              {tab}
            </button>
          ))}
        </div>
        <div ref={bodyRef} className="flex-1 overflow-y-auto p-4">
          {isLoading && (
            <div className="font-mono text-[11px] text-ink-3" role="status">
              Loading…
            </div>
          )}
          {isError && (
            <div className="font-mono text-[11px] text-rust" role="alert">
              Failed to load task.
            </div>
          )}
          {!isLoading && !isError && activeTab === "summary" && task && (
            <>
              <SummaryContent task={task} />
              {isTerminal(task.status) && (
                <ProofOfWorkCard
                  task={task}
                  prompts={prompts ?? []}
                  artifacts={artifacts ?? []}
                />
              )}
            </>
          )}
          {activeTab === "output" && (
            <>
              {streamError && (
                <div className="font-mono text-[11px] text-rust mb-2" role="alert">
                  Stream error: {streamError}
                </div>
              )}
              <pre className="font-mono text-[11px] text-ink whitespace-pre-wrap break-words">
                {streamText || <span className="text-ink-4">No output yet.</span>}
              </pre>
            </>
          )}
          {activeTab === "prompts" && (
            <RawJsonContent data={prompts} label="prompts" isError={isPromptsError} />
          )}
          {activeTab === "artifacts" && (
            <RawJsonContent data={artifacts} label="artifacts" isError={isArtifactsError} />
          )}
        </div>
      </div>
    </>
  );
}

function SummaryContent({ task }: { task: FullTask }) {
  return (
    <dl className="flex flex-col gap-2 font-mono text-[11px]">
      <Field label="Kind" value={task.task_kind} />
      <Field label="Status" value={task.status} />
      {task.phase && <Field label="Phase" value={task.phase} />}
      {task.repo && <Field label="Repo" value={task.repo} />}
      {task.description && <Field label="Description" value={task.description} />}
      {task.pr_url && (
        <div>
          <dt className="text-ink-3 mb-0.5">PR</dt>
          <dd>
            <a
              href={task.pr_url}
              target="_blank"
              rel="noreferrer"
              className="text-rust hover:underline break-all"
            >
              {task.pr_url}
            </a>
          </dd>
        </div>
      )}
      {task.error && <Field label="Error" value={task.error} />}
      {task.created_at && <Field label="Created" value={task.created_at} />}
    </dl>
  );
}

function Field({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <dt className="text-ink-3 mb-0.5">{label}</dt>
      <dd className="text-ink break-all">{value}</dd>
    </div>
  );
}

function RawJsonContent({ data, label, isError }: { data: unknown; label: string; isError?: boolean }) {
  if (isError) {
    return (
      <div className="font-mono text-[11px] text-rust" role="alert">
        Failed to load {label}.
      </div>
    );
  }
  if (data === undefined) {
    return <div className="font-mono text-[11px] text-ink-3">Loading {label}…</div>;
  }
  return (
    <pre className="font-mono text-[11px] text-ink whitespace-pre-wrap break-words">
      {JSON.stringify(data, null, 2)}
    </pre>
  );
}
