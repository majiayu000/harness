interface Props {
  label: string;
  intensity: number[];
}

function cellStyle(v: number): { background: string; borderColor: string } {
  const clamped = Math.max(0, Math.min(1, v));
  const a = clamped < 0.05 ? 0 : clamped < 0.35 ? 30 : clamped < 0.7 ? 60 : 100;
  if (a === 0) return { background: "var(--bg-2)", borderColor: "var(--line)" };
  return {
    background: `color-mix(in oklab,var(--rust) ${a}%,var(--bg-2))`,
    borderColor: "var(--line-2)",
  };
}

export function HeatmapRow({ label, intensity }: Props) {
  const active = intensity.filter((v) => v > 0.05).length;
  return (
    <div className="mb-3 last:mb-0">
      <div className="flex justify-between items-end font-mono text-[11px] text-ink-3 mb-1">
        <span className="text-ink-2">{label}</span>
        <span className="text-ink-4">{active} active bucket{active === 1 ? "" : "s"}</span>
      </div>
      <div className="grid gap-0.5" style={{ gridTemplateColumns: `repeat(${intensity.length}, 1fr)` }}>
        {intensity.map((v, i) => (
          <i key={i} className="aspect-square border" style={cellStyle(v)} />
        ))}
      </div>
    </div>
  );
}
