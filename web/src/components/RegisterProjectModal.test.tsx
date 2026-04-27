import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RegisterProjectModal } from "./RegisterProjectModal";

vi.mock("@/lib/queries", () => ({
  registerProject: vi.fn(),
}));

import { registerProject } from "@/lib/queries";
const mockRegisterProject = registerProject as ReturnType<typeof vi.fn>;

function wrap(ui: React.ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

const noop = () => {};

describe("<RegisterProjectModal>", () => {
  beforeEach(() => {
    mockRegisterProject.mockReset();
  });

  it("does not render when open=false", () => {
    wrap(<RegisterProjectModal open={false} onClose={noop} onSuccess={noop} />);
    expect(screen.queryByText(/register project/i)).not.toBeInTheDocument();
  });

  it("renders all form fields when open=true", () => {
    wrap(<RegisterProjectModal open={true} onClose={noop} onSuccess={noop} />);
    expect(screen.getByPlaceholderText("my-project")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("/srv/repos/my-project")).toBeInTheDocument();
    expect(screen.getByRole("combobox")).toBeInTheDocument();
    expect(screen.getByRole("spinbutton")).toBeInTheDocument();
  });

  it("shows inline validation errors for empty required fields", () => {
    wrap(<RegisterProjectModal open={true} onClose={noop} onSuccess={noop} />);
    fireEvent.click(screen.getByRole("button", { name: /register/i }));
    expect(screen.getByText(/project id is required/i)).toBeInTheDocument();
    expect(screen.getByText(/root path is required/i)).toBeInTheDocument();
    expect(mockRegisterProject).not.toHaveBeenCalled();
  });

  it("calls onSuccess and closes on successful submit", async () => {
    mockRegisterProject.mockResolvedValue(undefined);
    const onSuccess = vi.fn();
    const onClose = vi.fn();
    wrap(<RegisterProjectModal open={true} onClose={onClose} onSuccess={onSuccess} />);
    fireEvent.change(screen.getByPlaceholderText("my-project"), { target: { value: "my-proj" } });
    fireEvent.change(screen.getByPlaceholderText("/srv/repos/my-project"), {
      target: { value: "/srv/repos/my-proj" },
    });
    fireEvent.click(screen.getByRole("button", { name: /register/i }));
    await waitFor(() => {
      expect(onSuccess).toHaveBeenCalled();
      expect(onClose).toHaveBeenCalled();
    });
  });
});
