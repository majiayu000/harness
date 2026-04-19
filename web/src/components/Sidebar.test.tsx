import { describe, expect, it } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { render, screen } from "@testing-library/react";
import { Sidebar, SidebarSection } from "./Sidebar";

describe("<Sidebar>", () => {
  it("renders harness brand and env chip", () => {
    render(
      <MemoryRouter>
        <Sidebar env="local" sections={[]} />
      </MemoryRouter>,
    );
    expect(screen.getByText("harness")).toBeInTheDocument();
    expect(screen.getByText("local")).toBeInTheDocument();
  });

  it("renders section items with counts", () => {
    const sections: SidebarSection[] = [
      {
        label: "System",
        items: [
          { id: "overview", label: "Overview", href: "/overview", active: true },
          { id: "projects", label: "Projects", href: "/#projects", count: 4 },
        ],
      },
    ];
    render(
      <MemoryRouter>
        <Sidebar env="local" sections={sections} />
      </MemoryRouter>,
    );
    expect(screen.getByText("Overview")).toBeInTheDocument();
    expect(screen.getByText("4")).toBeInTheDocument();
  });
});
