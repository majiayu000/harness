import { describe, expect, it, vi, beforeEach } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { render, screen } from "@testing-library/react";
import { Dashboard } from "./Dashboard";
import { PaletteProvider } from "@/lib/palette";

vi.mock("@/lib/queries", () => ({
  useDashboard: vi.fn(),
  useTasks: vi.fn(),
}));

vi.mock("./dashboard/Active", () => ({
  Active: ({ selectedTaskId }: { selectedTaskId: string | null }) => <div>active {selectedTaskId}</div>,
}));

vi.mock("./dashboard/History", () => ({
  History: ({ selectedTaskId }: { selectedTaskId: string | null }) => <div>history {selectedTaskId}</div>,
}));

vi.mock("./dashboard/Channels", () => ({
  Channels: () => <div>channels</div>,
}));

vi.mock("./dashboard/Submit", () => ({
  Submit: () => <div>submit</div>,
}));

import { useDashboard, useTasks } from "@/lib/queries";

const mockUseDashboard = useDashboard as ReturnType<typeof vi.fn>;
const mockUseTasks = useTasks as ReturnType<typeof vi.fn>;

describe("<Dashboard>", () => {
  beforeEach(() => {
    mockUseDashboard.mockReturnValue({
      data: {
        onboarding: { phase: "complete" },
      },
      isError: false,
    });
    mockUseTasks.mockReturnValue({ data: [] });
  });

  it("routes first-run users into the guided submit flow", () => {
    mockUseDashboard.mockReturnValue({
      data: {
        onboarding: { phase: "register_project" },
      },
      isError: false,
    });

    render(
      <MemoryRouter>
        <PaletteProvider>
          <Dashboard />
        </PaletteProvider>
      </MemoryRouter>,
    );

    expect(screen.getByText("submit")).toBeInTheDocument();
  });

  it("opens the live board for ?task deep links", () => {
    mockUseTasks.mockReturnValue({
      data: [{ id: "task-1", status: "implementing" }],
    });

    render(
      <MemoryRouter initialEntries={["/?task=task-1"]}>
        <PaletteProvider>
          <Dashboard />
        </PaletteProvider>
      </MemoryRouter>,
    );

    expect(screen.getByText("active task-1")).toBeInTheDocument();
  });

  it("routes terminal deep links to history", () => {
    mockUseTasks.mockReturnValue({
      data: [{ id: "task-2", status: "done" }],
    });

    render(
      <MemoryRouter initialEntries={["/?task=task-2"]}>
        <PaletteProvider>
          <Dashboard />
        </PaletteProvider>
      </MemoryRouter>,
    );

    expect(screen.getByText("history task-2")).toBeInTheDocument();
  });
});
