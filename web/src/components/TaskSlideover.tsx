import { useEffect, useMemo, useState } from "react";
import { useTaskDetail, useTaskPrompts } from "@/lib/queries";

type TabId = "overview" | "prompts";

interface Props {
  taskId: string | null;
  open: boolean;
  onClose: () => void;
}

const TABS: { id: TabId; label: string }[] = [
  { id: "overview", label: "Overview" },
  { id: "prompts", label: "Prompts" },
];

function formatTimestamp(value: string | null | undefined): string {
  if (!value) return "Unknown time";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString();
}

export function TaskSlideover({ taskId, open, onClose }: Props) {
  const [activeTab, setActiveTab] = useState<TabId>("overview");

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [open, onClose]);

  useEffect(() => {
    setActiveTab("overview");
  }, [taskId]);

  const detailQuery = useTaskDetail(open ? taskId : null);
  const promptsQuery = useTaskPrompts(open && activeTab === "prompts" ? taskId : null);

  const title = useMemo(() => {
    const detail = detailQuery.data;
    if (!detail) return taskId?.slice(0, 8) ?? "Task";
    return detail.description?.trim() || detail.repo || detail.id.slice(0, 8);
  }, [detailQuery.data, taskId]);

  if (!open) return null;

  return (
    <>
      <button
        type="button"
        aria-label="Close task detail"
        className="fixed inset-0 z-[70] bg-[rgba(5,8,10,0.62)]"
        onClick={onClose}
      />
      <aside
        role="dialog"
        aria-modal="true"
        aria-label="Task detail"
        className="fixed inset-y-0 right-0 z-[80] flex w-full max-w-[880px] flex-col border-l border-line-2 bg-bg-1 shadow-[-24px_0_60px_rgba(0,0,0,.45)]"
      >
        <div className="flex items-start justify-between gap-4 border-b border-line px-5 py-4">
          <div className="min-w-0">
            <div className="font-mono text-[10.5px] uppercase tracking-[0.14em] text-ink-3">
              Task Detail
            </div>
            <h2 className="mt-1 text-sm font-medium leading-6 text-ink">{title}</h2>
            {taskId && <div className="mt-1 font-mono text-[11px] text-ink-3">{taskId}</div>}
          </div>
          <button
            type="button"
            onClick={onClose}
            className="border border-line px-2 py-1 font-mono text-[11px] uppercase tracking-[0.12em] text-ink-3 transition-colors hover:border-line-3 hover:text-ink"
          >
            Close
          </button>
        </div>

        <div className="border-b border-line px-5">
          <div className="flex gap-2 py-3">
            {TABS.map((tab) => (
              <button
                key={tab.id}
                type="button"
                onClick={() => setActiveTab(tab.id)}
                className={`border px-3 py-1.5 font-mono text-[11px] uppercase tracking-[0.12em] transition-colors ${
                  tab.id === activeTab
                    ? "border-rust bg-bg-2 text-rust"
                    : "border-line text-ink-3 hover:border-line-3 hover:text-ink"
                }`}
              >
                {tab.label}
              </button>
            ))}
          </div>
        </div>

        <div className="min-h-0 flex-1 overflow-auto px-5 py-4">
          {activeTab === "overview" && (
            <section className="space-y-4">
              {detailQuery.isLoading && (
                <div className="font-mono text-[11px] text-ink-3">Loading task…</div>
              )}
              {detailQuery.isError && (
                <div className="font-mono text-[11px] text-red-300">Unable to load task detail.</div>
              )}
              {detailQuery.data && (
                <>
                  <dl className="grid gap-3 sm:grid-cols-2">
                    <div className="border border-line bg-bg px-3 py-2">
                      <dt className="font-mono text-[10.5px] uppercase tracking-[0.12em] text-ink-3">
                        Status
                      </dt>
                      <dd className="mt-1 text-sm text-ink">{detailQuery.data.status}</dd>
                    </div>
                    <div className="border border-line bg-bg px-3 py-2">
                      <dt className="font-mono text-[10.5px] uppercase tracking-[0.12em] text-ink-3">
                        Phase
                      </dt>
                      <dd className="mt-1 text-sm text-ink">{detailQuery.data.phase ?? "—"}</dd>
                    </div>
                    <div className="border border-line bg-bg px-3 py-2">
                      <dt className="font-mono text-[10.5px] uppercase tracking-[0.12em] text-ink-3">
                        Turn
                      </dt>
                      <dd className="mt-1 text-sm text-ink">{detailQuery.data.turn}</dd>
                    </div>
                    <div className="border border-line bg-bg px-3 py-2">
                      <dt className="font-mono text-[10.5px] uppercase tracking-[0.12em] text-ink-3">
                        Created
                      </dt>
                      <dd className="mt-1 text-sm text-ink">
                        {formatTimestamp(detailQuery.data.created_at)}
                      </dd>
                    </div>
                  </dl>

                  <div className="border border-line bg-bg px-3 py-3">
                    <div className="font-mono text-[10.5px] uppercase tracking-[0.12em] text-ink-3">
                      Rounds
                    </div>
                    {detailQuery.data.rounds.length === 0 ? (
                      <div className="mt-2 font-mono text-[11px] text-ink-3">No rounds yet.</div>
                    ) : (
                      <div className="mt-3 space-y-2">
                        {detailQuery.data.rounds.map((round, index) => (
                          <div
                            key={`${round.turn}-${round.action}-${index}`}
                            className="border border-line px-3 py-2"
                          >
                            <div className="font-mono text-[11px] text-ink-2">
                              turn {round.turn} · {round.action}
                            </div>
                            <div className="mt-1 text-sm leading-6 text-ink">{round.result}</div>
                          </div>
                        ))}
                      </div>
                    )}
                  </div>
                </>
              )}
            </section>
          )}

          {activeTab === "prompts" && (
            <section className="space-y-4">
              {promptsQuery.isLoading && (
                <div className="font-mono text-[11px] text-ink-3">Loading prompts…</div>
              )}
              {promptsQuery.isError && (
                <div className="font-mono text-[11px] text-red-300">Unable to load prompts.</div>
              )}
              {!promptsQuery.isLoading && !promptsQuery.isError && promptsQuery.data?.length === 0 && (
                <div className="font-mono text-[11px] text-ink-3">No prompts recorded for this task.</div>
              )}
              {promptsQuery.data?.map((prompt) => (
                <article
                  key={`${prompt.turn}-${prompt.phase ?? "unknown"}-${prompt.created_at ?? "no-time"}`}
                  data-testid="task-prompt"
                  className="border border-line bg-bg px-3 py-3"
                >
                  <div className="flex flex-wrap items-center gap-x-3 gap-y-1 font-mono text-[10.5px] uppercase tracking-[0.12em] text-ink-3">
                    <span>{prompt.phase ?? "unknown phase"}</span>
                    <span>turn {prompt.turn}</span>
                    <span>{formatTimestamp(prompt.created_at)}</span>
                  </div>
                  <pre
                    aria-label={`Prompt body turn ${prompt.turn}`}
                    className="mt-3 max-h-[52vh] overflow-auto whitespace-pre-wrap break-words border border-line bg-bg-1 p-3 font-mono text-[11px] leading-5 text-ink"
                  >
                    {prompt.prompt}
                  </pre>
                </article>
              ))}
            </section>
          )}
        </div>
      </aside>
    </>
  );
}
