import type { OverviewFeedEntry } from "@/types";

const levelClass: Record<string, string> = {
  ok: "text-ok",
  warn: "text-warn",
  err: "text-danger",
  "": "text-rust",
};

interface Props {
  entries: OverviewFeedEntry[];
}

export function Feed({ entries }: Props) {
  if (!entries.length) {
    return <div className="font-mono text-[11px] text-ink-4 py-3">no events in window</div>;
  }
  return (
    <div className="px-4.5 pb-3 max-h-[460px] overflow-auto">
      {entries.map((f, idx) => (
        <div
          key={idx}
          className="grid grid-cols-[70px_1fr_auto] gap-3 py-2.5 border-b border-line items-start font-mono text-[11.5px] text-ink-2 last:border-b-0"
        >
          <span className="text-ink-4">{f.ago} ago</span>
          <div className="leading-[1.55]">
            <span className={`mr-1.5 ${levelClass[f.level] ?? ""}`}>{f.kind}</span>
            {f.body || f.tool}
          </div>
          <span className="font-mono text-[10px] text-ink-3 px-1.5 py-[1px] border border-line-2 rounded-[2px]">
            {f.project || "system"}
          </span>
        </div>
      ))}
    </div>
  );
}
