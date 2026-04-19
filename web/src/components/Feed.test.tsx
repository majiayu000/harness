import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { Feed } from "./Feed";

describe("<Feed>", () => {
  it("renders entries", () => {
    render(
      <Feed
        entries={[
          {
            ts: "2026-04-19T12:00:00Z",
            ago: "14s",
            kind: "task.done",
            tool: "claude",
            body: "task merged",
            level: "ok",
            project: "harness",
          },
        ]}
      />,
    );
    expect(screen.getByText(/task\.done/)).toBeInTheDocument();
    expect(screen.getByText(/task merged/)).toBeInTheDocument();
    expect(screen.getByText("harness")).toBeInTheDocument();
  });
  it("shows empty state", () => {
    render(<Feed entries={[]} />);
    expect(screen.getByText(/no events/i)).toBeInTheDocument();
  });
});
