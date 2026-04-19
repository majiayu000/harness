import { Sparkline } from "./Sparkline";

interface Props {
  label: string;
  value: string;
  unit?: string;
  delta?: string;
  sparkline?: number[];
}

export function KpiCard({ label, value, unit, delta, sparkline }: Props) {
  return (
    <div className="px-5 py-4 border-r border-line relative last:border-r-0">
      <div className="font-mono text-[10px] tracking-[0.1em] uppercase text-ink-3">{label}</div>
      <div className="font-mono text-2xl text-ink mt-1.5 tracking-[-0.01em] leading-none">
        {value}
        {unit && <small className="text-ink-3 text-[13px] ml-[3px] font-normal">{unit}</small>}
      </div>
      {delta && (
        <div className="font-mono text-[11px] mt-1.5 inline-flex items-center gap-1 text-ink-3">{delta}</div>
      )}
      {sparkline && (
        <div className="absolute right-4 top-4">
          <Sparkline values={sparkline} />
        </div>
      )}
    </div>
  );
}
