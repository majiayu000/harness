import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { RuntimeCard } from "./RuntimeCard";

describe("<RuntimeCard>", () => {
  it("renders display name and online state", () => {
    render(
      <RuntimeCard
        runtime={{
          id: "mac-1",
          display_name: "MacBook Pro",
          capabilities: ["claude", "codex"],
          online: true,
          last_heartbeat_at: "",
          active_leases: 4,
          watched_projects: 3,
          cpu_pct: null,
          ram_pct: null,
          tokens_24h: null,
        }}
      />,
    );
    expect(screen.getByText("MacBook Pro")).toBeInTheDocument();
    expect(screen.getByText(/online/i)).toBeInTheDocument();
  });
});
