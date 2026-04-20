import { describe, expect, it, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { PaletteProvider, usePalette, PALETTE_STORAGE_KEY } from "./palette";

function Probe() {
  const { current, setCurrent, palettes } = usePalette();
  return (
    <div>
      <span data-testid="current">{current}</span>
      <span data-testid="count">{palettes.length}</span>
      <button onClick={() => setCurrent("multica")}>multica</button>
    </div>
  );
}

describe("PaletteProvider", () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.removeAttribute("data-palette");
  });

  it("defaults to ember", () => {
    render(
      <PaletteProvider>
        <Probe />
      </PaletteProvider>,
    );
    expect(screen.getByTestId("current").textContent).toBe("ember");
    expect(document.documentElement.dataset.palette).toBe("ember");
  });

  it("reads persisted palette from localStorage", () => {
    localStorage.setItem(PALETTE_STORAGE_KEY, "bloom");
    render(
      <PaletteProvider>
        <Probe />
      </PaletteProvider>,
    );
    expect(screen.getByTestId("current").textContent).toBe("bloom");
    expect(document.documentElement.dataset.palette).toBe("bloom");
  });

  it("setCurrent updates state, attribute, and localStorage", () => {
    render(
      <PaletteProvider>
        <Probe />
      </PaletteProvider>,
    );
    fireEvent.click(screen.getByText("multica"));
    expect(screen.getByTestId("current").textContent).toBe("multica");
    expect(document.documentElement.dataset.palette).toBe("multica");
    expect(localStorage.getItem(PALETTE_STORAGE_KEY)).toBe("multica");
  });

  it("ignores unknown palette strings", () => {
    localStorage.setItem(PALETTE_STORAGE_KEY, "does-not-exist");
    render(
      <PaletteProvider>
        <Probe />
      </PaletteProvider>,
    );
    expect(screen.getByTestId("current").textContent).toBe("ember");
  });

  it("exposes 11 palettes", () => {
    render(
      <PaletteProvider>
        <Probe />
      </PaletteProvider>,
    );
    expect(screen.getByTestId("count").textContent).toBe("11");
  });
});
