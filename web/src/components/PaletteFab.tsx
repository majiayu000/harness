import { useState } from "react";
import { PaletteDrawer } from "./PaletteDrawer";

export function PaletteFab() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        onClick={() => setOpen((v) => !v)}
        title="palette"
        className="fixed right-5 bottom-5 z-[90] inline-flex items-center gap-2 px-3 py-[9px] border border-line-2 bg-bg-1 text-ink-2 font-mono text-[11.5px] cursor-pointer rounded-[3px] shadow-[0_6px_24px_rgba(0,0,0,.35)] hover:text-rust hover:border-rust"
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" className="w-[13px] h-[13px]">
          <circle cx="12" cy="12" r="9" />
          <path d="M12 3a9 9 0 000 18M3 12h18" />
        </svg>
        palette
      </button>
      <PaletteDrawer open={open} onClose={() => setOpen(false)} />
    </>
  );
}
