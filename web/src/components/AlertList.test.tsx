import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { AlertList } from "./AlertList";

describe("<AlertList>", () => {
  it("renders alert message and sub", () => {
    render(
      <AlertList
        alerts={[{ level: "warn", msg: "1 runtime offline", sub: "linux-server", ts: null }]}
      />,
    );
    expect(screen.getByText("1 runtime offline")).toBeInTheDocument();
    expect(screen.getByText("linux-server")).toBeInTheDocument();
  });
  it("shows empty state", () => {
    render(<AlertList alerts={[]} />);
    expect(screen.getByText(/no open alerts/i)).toBeInTheDocument();
  });
});
