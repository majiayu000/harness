import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { Channels } from "./Channels";

vi.mock("@/lib/queries", () => ({
  useDashboard: vi.fn(),
}));

import { useDashboard } from "@/lib/queries";

const mockUseDashboard = useDashboard as ReturnType<typeof vi.fn>;

function makeHost(id: string, displayName: string, roots: string[]) {
  return {
    id,
    display_name: displayName,
    capabilities: [],
    online: true,
    last_heartbeat_at: "",
    watched_projects: roots.length,
    watched_project_roots: roots,
    active_leases: 0,
    assignment_pressure: 0,
  };
}

const hosts = [
  makeHost("host-a", "Host Alpha", ["/srv/repos/harness", "/srv/repos/other"]),
  makeHost("host-b", "Host Beta", ["/srv/repos/other"]),
  makeHost("host-c", "Host Gamma", []),
];

const projects = [
  { id: "harness", root: "/srv/repos/harness", tasks: { running: 0, queued: 0, done: 0, failed: 0 }, latest_pr: null },
  { id: "other", root: "/srv/repos/other", tasks: { running: 0, queued: 0, done: 0, failed: 0 }, latest_pr: null },
];

beforeEach(() => {
  mockUseDashboard.mockReturnValue({
    data: { runtime_hosts: hosts, projects, global: {}, llm_metrics: {} },
  });
});

describe("<Channels>", () => {
  it("shows all hosts when no projectFilter", () => {
    render(<Channels />);
    expect(screen.getByText("Host Alpha")).toBeInTheDocument();
    expect(screen.getByText("Host Beta")).toBeInTheDocument();
    expect(screen.getByText("Host Gamma")).toBeInTheDocument();
  });

  it("filters to hosts watching the resolved project root", () => {
    render(<Channels projectFilter="harness" />);
    expect(screen.getByText("Host Alpha")).toBeInTheDocument();
    expect(screen.queryByText("Host Beta")).not.toBeInTheDocument();
    expect(screen.queryByText("Host Gamma")).not.toBeInTheDocument();
  });

  it("shows hosts for another project", () => {
    render(<Channels projectFilter="other" />);
    expect(screen.getByText("Host Alpha")).toBeInTheDocument();
    expect(screen.getByText("Host Beta")).toBeInTheDocument();
    expect(screen.queryByText("Host Gamma")).not.toBeInTheDocument();
  });

  it("shows empty message when no hosts match filter", () => {
    render(<Channels projectFilter="nonexistent" />);
    expect(screen.queryByText("Host Alpha")).not.toBeInTheDocument();
    expect(screen.getByText(/no runtime hosts connected/)).toBeInTheDocument();
  });
});
