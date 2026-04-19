import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { TopBar } from "./TopBar";

describe("<TopBar>", () => {
  it("renders breadcrumb and search placeholder", () => {
    render(
      <TopBar
        breadcrumb={[{ label: "system" }, { label: "overview", current: true }]}
        searchPlaceholder="Search projects…"
      />,
    );
    expect(screen.getByText("system")).toBeInTheDocument();
    expect(screen.getByText("overview")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("Search projects…")).toBeInTheDocument();
  });
});
