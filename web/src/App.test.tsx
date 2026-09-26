import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { App } from "./App";

describe("Console routes", () => {
  it("opens a Console view from the root query without using the project API path", () => {
    render(
      <MemoryRouter initialEntries={["/?view=projects&theme=light"]}>
        <App />
      </MemoryRouter>,
    );

    expect(screen.getByTitle("Harness Console")).toHaveAttribute(
      "src",
      "/console-v2/index.html?view=projects&theme=light&screen=projects",
    );
  });

  it("opens the v3 Events view from a deep link", () => {
    render(<MemoryRouter initialEntries={["/?view=events"]}><App /></MemoryRouter>);
    expect(screen.getByTitle("Harness Console")).toHaveAttribute("src", "/console-v2/index.html?view=events&screen=events");
  });

  it("keeps an existing usage deep link on the usage view", () => {
    render(
      <MemoryRouter initialEntries={["/usage"]}>
        <App />
      </MemoryRouter>,
    );

    expect(screen.getByTitle("Harness Console")).toHaveAttribute(
      "src",
      "/console-v2/index.html?screen=usage",
    );
  });
});
