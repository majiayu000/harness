interface Props {
  values: number[];
  bars?: number;
}

export function Sparkline({ values, bars = 24 }: Props) {
  if (!values.length) {
    return (
      <div className="flex gap-0.5 h-[22px] items-end">
        {Array.from({ length: bars }).map((_, i) => (
          <i key={i} className="block w-[3px] h-1 opacity-50 bg-rust/60" />
        ))}
      </div>
    );
  }
  const max = Math.max(...values, 1);
  return (
    <div className="flex gap-0.5 h-[22px] items-end">
      {values.map((v, i) => (
        <i
          key={i}
          className="block w-[3px] bg-rust/60"
          style={{ height: `${Math.max(2, Math.round((v / max) * 19))}px` }}
        />
      ))}
    </div>
  );
}
