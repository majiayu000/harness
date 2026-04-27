import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { EvolutionCard } from "./EvolutionCard";

describe("<EvolutionCard>", () => {
  it("shows dashes for all values when evolution is null", () => {
    render(<EvolutionCard evolution={null} />);
    const dashes = screen.getAllByText("—");
    expect(dashes.length).toBe(3);
  });

  it("renders the three counter values when evolution data is present", () => {
    render(
      <EvolutionCard
        evolution={{ drafts_pending: 4, drafts_auto_adopted: 2, skills_invoked_in_window: 11 }}
      />,
    );
    expect(screen.getByText("4")).toBeInTheDocument();
    expect(screen.getByText("2")).toBeInTheDocument();
    expect(screen.getByText("11")).toBeInTheDocument();
  });

  it("renders the card title", () => {
    render(<EvolutionCard evolution={null} />);
    expect(screen.getByText(/self-evolution/i)).toBeInTheDocument();
  });
});
