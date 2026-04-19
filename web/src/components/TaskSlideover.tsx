import { useEffect, useRef, useState } from "react";
import { useTask, useTaskArtifacts, useTaskPrompts } from "@/lib/queries";
import { useTaskStream } from "@/lib/useTaskStream";
import type { TaskArtifact, TaskPrompt } from "@/types";

const TABS = ["Stream", "Diff", "Review", "Events"] as const;
type Tab = (typeof TABS)[number];

function DiffView({ artifacts }: { artifacts: TaskArtifact[] }) {
  const diffs = artifacts.filter((a) => a.artifact_type === "diff");
  if (diffs.length === 0) {
    return <div className="p-3 font-mono text-[11px] text-ink-4">—</div>;
  }
  return (
    <div className="p-2 space-y-4">
      {diffs.map((a, i) => (
        <div key={i} className="font-mono text-[11px] leading-relaxed">
          {a.content.split("\n").map((line, j) => {
            let cls = "text-ink";
            if (line.startsWith("+")) cls = "text-ok";
            else if (line.startsWith("-")) cls = "text-danger";
            else if (line.startsWith("@@")) cls = "text-ink-3";
            return (
              <div key={j} className={cls}>
                {line || "\u00a0"}
              </div>
            );
          })}
        </div>
      ))}
    </div>
  );
}

function ReviewView({ prompts }: { prompts: TaskPrompt[] }) {
  const reviews = prompts.filter(
    (p) => p.phase === "review" || p.phase === "cross_review",
  );
  if (reviews.length === 0) {
    return <div className="p-3 font-mono text-[11px] text-ink-4">—</div>;
  }
  return (
    <div className="p-2 space-y-4">
      {reviews.map((p, i) => (
        <pre key={i} className="font-mono text-[11px] text-ink whitespace-pre-wrap break-words">
          {p.prompt}
        </pre>
      ))}
    </div>
  );
}

interface Props {
  taskId: string | null;
  onClose: () => void;
}

export function TaskSlideover({ taskId, onClose }: Props) {
  const [activeTab, setActiveTab] = useState<Tab>("Stream");
  const { lines, connected } = useTaskStream(taskId);
  const { data: task } = useTask(taskId);
  const { data: artifacts } = useTaskArtifacts(taskId);
  const { data: prompts } = useTaskPrompts(taskId);
  const streamEndRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [onClose]);

  useEffect(() => {
    if (activeTab === "Stream" && typeof streamEndRef.current?.scrollIntoView === "function") {
      streamEndRef.current.scrollIntoView({ behavior: "smooth" });
    }
  }, [lines, activeTab]);

  if (!taskId) return null;

  return (
    <>
      <div
        role="presentation"
        className="fixed inset-0 z-40 bg-black/30 backdrop-blur-sm"
        onClick={onClose}
        aria-hidden="true"
      />
      <div className="fixed top-0 right-0 bottom-0 z-50 w-[480px] bg-bg-1 border-l border-line flex flex-col shadow-[−24px_0_60px_rgba(0,0,0,.45)]">
        <div className="flex items-center justify-between px-3.5 py-3 border-b border-line flex-none">
          <span className="font-mono text-[11px] text-ink-3 tracking-[0.12em] uppercase truncate pr-2">
            {taskId.slice(0, 8)}
          </span>
          <button
            onClick={onClose}
            aria-label="Close"
            className="text-ink-3 text-lg leading-none hover:text-ink transition-colors"
          >
            ×
          </button>
        </div>
        <div className="flex border-b border-line flex-none" role="tablist">
          {TABS.map((tab) => (
            <button
              key={tab}
              role="tab"
              aria-selected={activeTab === tab}
              onClick={() => setActiveTab(tab)}
              className={`px-3.5 py-2 font-mono text-[10.5px] tracking-[0.08em] uppercase border-b-2 transition-colors ${
                activeTab === tab
                  ? "border-rust text-ink"
                  : "border-transparent text-ink-3 hover:text-ink-2"
              }`}
            >
              {tab}
            </button>
          ))}
        </div>
        <div className="flex-1 overflow-auto">
          {activeTab === "Stream" && (
            <pre className="p-3 font-mono text-[11px] text-ink whitespace-pre-wrap break-words">
              {!connected && lines.length === 0 && (
                <span className="text-ink-4">connecting…</span>
              )}
              {lines.join("\n")}
              <div ref={streamEndRef} />
            </pre>
          )}
          {activeTab === "Diff" && <DiffView artifacts={artifacts ?? []} />}
          {activeTab === "Review" && <ReviewView prompts={prompts ?? []} />}
          {activeTab === "Events" && (
            <div className="p-3 space-y-2">
              {task ? (
                <>
                  <div className="flex items-center gap-2 font-mono text-[11px]">
                    <span className="text-ink-3">status</span>
                    <span className="text-ink">{task.status}</span>
                  </div>
                  <div className="flex items-center gap-2 font-mono text-[11px]">
                    <span className="text-ink-3">turn</span>
                    <span className="text-ink">{task.turn}</span>
                  </div>
                  {task.phase && (
                    <div className="flex items-center gap-2 font-mono text-[11px]">
                      <span className="text-ink-3">phase</span>
                      <span className="text-ink">{task.phase}</span>
                    </div>
                  )}
                  {task.error && (
                    <div className="flex items-start gap-2 font-mono text-[11px]">
                      <span className="text-ink-3 flex-none">error</span>
                      <span className="text-danger break-words">{task.error}</span>
                    </div>
                  )}
                </>
              ) : (
                <div className="text-ink-4 font-mono text-[11px]">loading…</div>
              )}
            </div>
          )}
        </div>
      </div>
    </>
  );
}
