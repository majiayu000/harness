import type { OverviewAlert } from "@/types";

const icons: Record<string, React.ReactNode> = {
  warn: (
    <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.8">
      <path d="M12 3l10 18H2z" />
      <path d="M12 10v5M12 18v.5" />
    </svg>
  ),
  err: (
    <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.8">
      <circle cx="12" cy="12" r="9" />
      <path d="M8 8l8 8M16 8l-8 8" />
    </svg>
  ),
  ok: (
    <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.8">
      <path d="M4 12l5 5L20 7" />
    </svg>
  ),
};

const iconClass: Record<string, string> = {
  warn: "text-warn border-warn/40",
  err: "text-danger border-danger/40",
  ok: "text-ok border-ok/40",
};

interface Props {
  alerts: OverviewAlert[];
}

export function AlertList({ alerts }: Props) {
  if (!alerts.length) {
    return <div className="font-mono text-[11px] text-ink-4 py-2.5">no open alerts</div>;
  }
  return (
    <div className="px-4.5 pb-3">
      {alerts.map((a, i) => (
        <div
          key={i}
          className="py-2.5 border-b border-line grid grid-cols-[24px_1fr_auto] gap-3 font-mono text-[11.5px] last:border-b-0"
        >
          <div className={`w-[22px] h-[22px] border grid place-items-center flex-none ${iconClass[a.level] ?? iconClass.warn}`}>
            {icons[a.level] ?? icons.warn}
          </div>
          <div className="text-ink leading-[1.45]">
            {a.msg}
            <div className="text-ink-3 text-[10.5px] mt-0.5">{a.sub}</div>
          </div>
          <span className="text-ink-4 text-[10.5px] self-center">{a.ts ?? ""}</span>
        </div>
      ))}
    </div>
  );
}
