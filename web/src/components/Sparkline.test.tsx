import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { Sparkline } from "./Sparkline";

describe("<Sparkline>", () => {
  it("renders one bar per value", () => {
    const { container } = render(<Sparkline values={[1, 2, 3, 4]} />);
    expect(container.querySelectorAll("i").length).toBe(4);
  });
  it("renders placeholder when empty", () => {
    const { container } = render(<Sparkline values={[]} />);
    expect(container.querySelectorAll("i").length).toBe(24);
  });
});
