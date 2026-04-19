import type { OverviewRuntime } from "@/types";
import { fmtInt } from "@/lib/format";

interface Props {
  runtime: OverviewRuntime;
}

function classify(rt: OverviewRuntime): "mac" | "linux" | "cloud" | "server" {
  const n = `${rt.display_name} ${rt.id}`.toLowerCase();
  if (n.includes("mac") || n.includes("book")) return "mac";
  if (n.includes("linux") || n.includes("ubuntu")) return "linux";
  if (n.includes("api") || n.includes("cloud") || n.includes("anthropic") || n.includes("openai")) return "cloud";
  return "server";
}

const ICONS: Record<string, React.ReactNode> = {
  mac: <g><rect x="3" y="5" width="18" height="12" rx="1" /><path d="M2 20h20" /></g>,
  linux: <g><rect x="4" y="3" width="16" height="8" rx="1" /><rect x="4" y="13" width="16" height="8" rx="1" /></g>,
  cloud: <path d="M7 18a5 5 0 110-10 6 6 0 0111 3.5A4 4 0 0117 18z" />,
  server: <g><rect x="3" y="5" width="18" height="14" rx="1" /><path d="M7 10h3M7 14h3M14 10h3" /></g>,
};

export function RuntimeCard({ runtime }: Props) {
  const kind = classify(runtime);
  const hasPerf =
    typeof runtime.cpu_pct === "number" &&
    Number.isFinite(runtime.cpu_pct) &&
    typeof runtime.ram_pct === "number" &&
    Number.isFinite(runtime.ram_pct);
  return (
    <div className="border border-line bg-bg-1 p-3.5 cursor-pointer transition-colors hover:border-line-3">
      <div className="flex items-center gap-2.5 mb-2.5">
        <div className="w-7 h-7 grid place-items-center border border-line-2 text-rust flex-none">
          <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" strokeWidth="1.6">
            {ICONS[kind]}
          </svg>
        </div>
        <div>
          <div className="text-[13.5px] font-medium">{runtime.display_name}</div>
          <div className="font-mono text-[11px] text-ink-3">{runtime.id}</div>
        </div>
        <div
          className={`ml-auto inline-flex items-center gap-1.5 font-mono text-[11px] px-2 py-[3px] border rounded-[3px] ${
            runtime.online
              ? "text-ok border-ok/40"
              : "text-ink-4 border-line-2"
          }`}
        >
          <i className={`w-[7px] h-[7px] rounded-full ${runtime.online ? "bg-ok" : "bg-ink-4"}`} />
          {runtime.online ? "online" : "offline"}
        </div>
      </div>

      <div className="font-mono text-[11px] text-ink-3 grid grid-cols-2 gap-y-1 gap-x-3">
        <div className="flex justify-between"><span>active leases</span><b className="text-ink-2 font-medium">{fmtInt(runtime.active_leases)}</b></div>
        <div className="flex justify-between"><span>watched</span><b className="text-ink-2 font-medium">{fmtInt(runtime.watched_projects)}</b></div>
        {hasPerf ? (
          <>
            <div className="flex justify-between"><span>cpu</span><b className="text-ink-2 font-medium">{runtime.cpu_pct}%</b></div>
            <div className="flex justify-between"><span>ram</span><b className="text-ink-2 font-medium">{runtime.ram_pct}%</b></div>
            <div className="col-span-2 h-1 bg-bg-2 border border-line overflow-hidden my-0.5">
              <i className="block h-full bg-gradient-to-r from-rust to-rust-deep" style={{ width: `${runtime.cpu_pct}%` }} />
            </div>
          </>
        ) : (
          <>
            <div className="flex justify-between"><span>capabilities</span><b className="text-ink-2 font-medium">{runtime.capabilities.length}</b></div>
            <div className="flex justify-between"><span>kind</span><b className="text-ink-2 font-medium">{kind}</b></div>
          </>
        )}
      </div>

      <div className="flex gap-1 flex-wrap mt-2.5">
        {runtime.capabilities.map((c) => (
          <span key={c} className="font-mono text-[10px] px-1.5 py-[1px] border border-line-2 rounded-[3px] text-ink-3">
            {c}
          </span>
        ))}
      </div>
    </div>
  );
}
