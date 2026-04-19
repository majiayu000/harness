import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { StatusBadge } from "./StatusBadge";

describe("<StatusBadge>", () => {
  it("renders green with 'all systems nominal' when ok", () => {
    render(<StatusBadge ok />);
    expect(screen.getByText(/all systems nominal/i)).toBeInTheDocument();
  });
  it("renders red with 'connection lost' when not ok", () => {
    render(<StatusBadge ok={false} />);
    expect(screen.getByText(/connection lost/i)).toBeInTheDocument();
  });
});
