import { describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import { PaletteProvider } from "@/lib/palette";
import { DOCS_URL } from "@/lib/links";
import { Worktrees } from "./Worktrees";

vi.mock("@/lib/queries", () => ({
  useWorktrees: vi.fn(),
  useOverview: vi.fn(),
}));

import { useOverview, useWorktrees } from "@/lib/queries";

const mockUseWorktrees = useWorktrees as ReturnType<typeof vi.fn>;
const mockUseOverview = useOverview as ReturnType<typeof vi.fn>;

function wrap(ui: React.ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <PaletteProvider>
        <MemoryRouter>{ui}</MemoryRouter>
      </PaletteProvider>
    </QueryClientProvider>,
  );
}

describe("<Worktrees>", () => {
  it("links Docs to the repository documentation", () => {
    mockUseWorktrees.mockReturnValue({
      cards: [],
      isLoading: false,
      error: null,
    });
    mockUseOverview.mockReturnValue({
      data: {
        projects: [],
        runtimes: [],
        kpi: {
          active_tasks: 0,
        },
      },
    });

    wrap(<Worktrees />);

    expect(screen.getByRole("link", { name: "Docs" })).toHaveAttribute("href", DOCS_URL);
  });
});
