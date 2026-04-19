import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { KpiCard } from "./KpiCard";

describe("<KpiCard>", () => {
  it("renders label, value, delta", () => {
    render(<KpiCard label="Active tasks" value="47" delta="▲ 12 vs 24h" />);
    expect(screen.getByText("Active tasks")).toBeInTheDocument();
    expect(screen.getByText("47")).toBeInTheDocument();
    expect(screen.getByText("▲ 12 vs 24h")).toBeInTheDocument();
  });
  it("renders value with unit suffix", () => {
    render(<KpiCard label="Tokens" value="84.2" unit="M" />);
    expect(screen.getByText("84.2")).toBeInTheDocument();
    expect(screen.getByText("M")).toBeInTheDocument();
  });
});
