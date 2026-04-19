import { usePalette } from "@/lib/palette";

interface Props {
  open: boolean;
  onClose: () => void;
}

export function PaletteDrawer({ open, onClose }: Props) {
  const { current, setCurrent, palettes } = usePalette();
  if (!open) return null;
  return (
    <div className="fixed right-5 bottom-[66px] z-[95] w-[300px] bg-bg-1 border border-line-2 font-sans shadow-[0_24px_60px_rgba(0,0,0,.55)]">
      <div className="flex justify-between items-center px-3.5 py-3 border-b border-line font-mono text-[11px] text-ink-3 tracking-[0.12em] uppercase">
        <span>Palette</span>
        <button onClick={onClose} className="text-ink-3 text-lg">×</button>
      </div>
      <div className="p-2 flex flex-col gap-[3px] max-h-[60vh] overflow-auto">
        {palettes.map((p) => (
          <button
            key={p.id}
            onClick={() => setCurrent(p.id)}
            className={`flex items-center gap-3 px-2.5 py-[7px] bg-transparent border text-left text-ink-2 ${
              p.id === current ? "border-rust bg-bg-2" : "border-transparent hover:bg-bg-2 hover:border-line"
            }`}
          >
            <span className="flex gap-0.5 flex-none">
              {p.swatch.map((c, i) => (
                <i key={i} className="w-[13px] h-6 block" style={{ background: c }} />
              ))}
            </span>
            <div>
              <div className="font-mono text-[12px] text-ink font-medium">{p.name}</div>
              <div className="font-mono text-[10.5px] text-ink-3 mt-0.5">{p.sub}</div>
            </div>
          </button>
        ))}
      </div>
    </div>
  );
}
