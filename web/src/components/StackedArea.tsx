export interface StackedAreaSeries {
  name: string;
  values: number[];
  color: string;
}

interface Props {
  series: StackedAreaSeries[];
  width?: number;
  height?: number;
}

export function StackedArea({ series, width = 900, height = 200 }: Props) {
  const active = series.filter((s) => s.values.length > 0);
  const N = active[0]?.values.length ?? 0;
  if (!N) {
    return (
      <div className="p-3 text-ink-4 font-mono text-[11px]">no data in window</div>
    );
  }

  const stacked = new Array(N).fill(0) as number[];
  const layers = active.map((s) =>
    s.values.map((v, i) => {
      const bottom = stacked[i];
      stacked[i] += v;
      return { bottom, top: stacked[i] };
    }),
  );
  const maxY = Math.max(...stacked, 1) * 1.15;
  const x = (i: number) => (i / Math.max(1, N - 1)) * width;
  const y = (v: number) => height - (v / maxY) * height;

  return (
    <svg viewBox={`0 0 ${width} ${height}`} preserveAspectRatio="none" className="w-full h-full overflow-visible">
      {[0, 1, 2, 3, 4].map((i) => (
        <line key={i} x1={0} x2={width} y1={(i / 4) * height} y2={(i / 4) * height} stroke="rgba(128,128,128,.08)" />
      ))}
      {layers.map((layer, si) => {
        let path = `M ${x(0)} ${y(layer[0].bottom)} `;
        for (let i = 0; i < N; i++) path += `L ${x(i)} ${y(layer[i].top)} `;
        for (let i = N - 1; i >= 0; i--) path += `L ${x(i)} ${y(layer[i].bottom)} `;
        path += "Z";
        return (
          <path
            key={active[si].name}
            d={path}
            fill={active[si].color}
            fillOpacity={0.85}
            stroke={active[si].color}
            strokeWidth={0.8}
          />
        );
      })}
    </svg>
  );
}
