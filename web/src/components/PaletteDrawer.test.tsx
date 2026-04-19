import { describe, expect, it } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { PaletteProvider } from "@/lib/palette";
import { PaletteDrawer } from "./PaletteDrawer";

describe("<PaletteDrawer>", () => {
  it("lists all 10 palettes", () => {
    render(
      <PaletteProvider>
        <PaletteDrawer open onClose={() => {}} />
      </PaletteProvider>,
    );
    for (const name of ["Ember", "Forge", "Ink", "Bloom", "Terminal", "Mint", "Linen", "Porcelain", "Pixel", "Multica"]) {
      expect(screen.getByText(name)).toBeInTheDocument();
    }
  });
  it("clicking a palette switches data-palette", () => {
    render(
      <PaletteProvider>
        <PaletteDrawer open onClose={() => {}} />
      </PaletteProvider>,
    );
    fireEvent.click(screen.getByText("Multica"));
    expect(document.documentElement.dataset.palette).toBe("multica");
  });
});
