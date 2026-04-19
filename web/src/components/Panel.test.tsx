import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { Panel } from "./Panel";

describe("<Panel>", () => {
  it("renders title, sub, and children", () => {
    render(
      <Panel title="Fleet throughput" sub="per hour">
        <div>body</div>
      </Panel>,
    );
    expect(screen.getByText("Fleet throughput")).toBeInTheDocument();
    expect(screen.getByText("per hour")).toBeInTheDocument();
    expect(screen.getByText("body")).toBeInTheDocument();
  });
});
