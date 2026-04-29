import { describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { fireEvent, render, screen } from "@testing-library/react";
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

  it("invokes onItemClick with the item id for items without an href", () => {
    const onItemClick = vi.fn();
    const sections: SidebarSection[] = [
      {
        label: "Operations",
        items: [
          { id: "history", label: "History" },
          { id: "channels", label: "Channels" },
        ],
      },
    ];
    render(
      <MemoryRouter>
        <Sidebar env="local" sections={sections} onItemClick={onItemClick} />
      </MemoryRouter>,
    );
    fireEvent.click(screen.getByText("History"));
    expect(onItemClick).toHaveBeenCalledWith("history");
  });

  it("renders absolute hrefs as external links", () => {
    const sections: SidebarSection[] = [
      {
        label: "Reference",
        items: [{ id: "docs", label: "Docs", href: "https://example.com/docs" }],
      },
    ];
    render(
      <MemoryRouter>
        <Sidebar env="local" sections={sections} />
      </MemoryRouter>,
    );

    const link = screen.getByRole("link", { name: "Docs" });
    expect(link).toHaveAttribute("href", "https://example.com/docs");
    expect(link).toHaveAttribute("target", "_blank");
    expect(link).toHaveAttribute("rel", "noreferrer");
  });
});
