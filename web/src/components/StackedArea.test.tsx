import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { StackedArea } from "./StackedArea";

describe("<StackedArea>", () => {
  it("renders one path per series", () => {
    const { container } = render(
      <StackedArea
        series={[
          { name: "a", values: [1, 2, 3], color: "var(--rust)" },
          { name: "b", values: [2, 1, 4], color: "var(--moss)" },
        ]}
      />,
    );
    expect(container.querySelectorAll("path").length).toBe(2);
  });
  it("renders empty-state note when no values", () => {
    const { getByText } = render(<StackedArea series={[]} />);
    expect(getByText(/no data/i)).toBeInTheDocument();
  });
});
