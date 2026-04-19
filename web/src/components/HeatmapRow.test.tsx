import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { HeatmapRow } from "./HeatmapRow";

describe("<HeatmapRow>", () => {
  it("renders one cell per intensity value", () => {
    const { container } = render(<HeatmapRow label="harness" intensity={[0, 0.3, 0.6, 0.9]} />);
    expect(container.querySelectorAll(".grid > i").length).toBe(4);
  });
});
