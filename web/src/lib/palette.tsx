import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";

export const PALETTE_STORAGE_KEY = "harness.palette";

export type PaletteId =
  | "ember"
  | "forge"
  | "ink"
  | "bloom"
  | "terminal"
  | "mint"
  | "linen"
  | "porcelain"
  | "pixel"
  | "multica";

export interface PaletteDescriptor {
  id: PaletteId;
  name: string;
  sub: string;
  swatch: [string, string, string, string];
}

export const PALETTES: readonly PaletteDescriptor[] = [
  { id: "ember", name: "Ember", sub: "warm · rust · original", swatch: ["#0e0c09", "#1c1812", "#e26a2c", "#f1ebe0"] },
  { id: "forge", name: "Forge", sub: "steel · amber", swatch: ["#0a0c10", "#1f242e", "#f5a524", "#e6ebf4"] },
  { id: "ink", name: "Ink", sub: "deep · cyan", swatch: ["#06070a", "#181c26", "#53d4ff", "#eef2f8"] },
  { id: "bloom", name: "Bloom", sub: "violet · magenta", swatch: ["#0c0a12", "#231d2f", "#ff5fae", "#f3edfb"] },
  { id: "terminal", name: "Terminal", sub: "classic green phosphor", swatch: ["#060a06", "#152115", "#7aff7a", "#c7f4bf"] },
  { id: "mint", name: "Mint", sub: "light · paper", swatch: ["#f4efe6", "#d4c9af", "#1f7a54", "#1e1b16"] },
  { id: "linen", name: "Linen", sub: "warm paper · indigo", swatch: ["#fbf7ee", "#ddd0b0", "#2747cf", "#141820"] },
  { id: "porcelain", name: "Porcelain", sub: "neutral · crimson", swatch: ["#f6f5f2", "#d0cec3", "#d84a2f", "#1a1a1a"] },
  { id: "pixel", name: "Pixel", sub: "muted · 8-bit amber", swatch: ["#14161f", "#2f3448", "#f4b860", "#e8ecf4"] },
  { id: "multica", name: "Multica", sub: "cosmic · aurora", swatch: ["#0a0b14", "#1f2340", "#ff7a59", "#f2f1fb"] },
] as const;

const VALID_IDS = new Set<string>(PALETTES.map((p) => p.id));
const DEFAULT_PALETTE: PaletteId = "ember";

interface PaletteContextValue {
  current: PaletteId;
  setCurrent: (id: PaletteId) => void;
  palettes: readonly PaletteDescriptor[];
}

const PaletteContext = createContext<PaletteContextValue | null>(null);

function readInitial(): PaletteId {
  try {
    const raw = localStorage.getItem(PALETTE_STORAGE_KEY);
    if (raw && VALID_IDS.has(raw)) return raw as PaletteId;
  } catch {
    // ignore storage failures
  }
  return DEFAULT_PALETTE;
}

export function PaletteProvider({ children }: { children: ReactNode }) {
  const [current, setCurrentState] = useState<PaletteId>(readInitial);

  useEffect(() => {
    document.documentElement.dataset.palette = current;
  }, [current]);

  const setCurrent = useCallback((id: PaletteId) => {
    setCurrentState(id);
    try {
      localStorage.setItem(PALETTE_STORAGE_KEY, id);
    } catch {
      // ignore storage failures
    }
  }, []);

  const value = useMemo(
    () => ({ current, setCurrent, palettes: PALETTES }),
    [current, setCurrent],
  );

  return <PaletteContext.Provider value={value}>{children}</PaletteContext.Provider>;
}

export function usePalette(): PaletteContextValue {
  const ctx = useContext(PaletteContext);
  if (!ctx) throw new Error("usePalette must be used inside <PaletteProvider>");
  return ctx;
}
