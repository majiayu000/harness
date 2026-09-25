import { useEffect } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Route, Routes, useLocation } from "react-router-dom";
import { TokenPrompt } from "./components/TokenPrompt";
import { unauthorizedEvents } from "./lib/api";

type ConsoleScreen =
  | "home"
  | "fleet"
  | "projects"
  | "history"
  | "worktrees"
  | "usage"
  | "library"
  | "system";

const queryClient = new QueryClient();

function Console({ screen }: { screen: ConsoleScreen }) {
  const { search } = useLocation();
  const params = new URLSearchParams(search);
  const view = params.get("view");
  const selected: ConsoleScreen =
    view === "home" || view === "fleet" || view === "projects" ||
    view === "history" || view === "worktrees" || view === "usage" ||
    view === "library" || view === "system" ? view : screen;
  params.set("screen", selected);
  useEffect(() => {
    const onMessage = (event: MessageEvent) => {
      if (event.origin !== window.location.origin) return;
      if (event.data?.type === "harness:unauthorized") {
        unauthorizedEvents.dispatchEvent(new Event("unauthorized"));
      }
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, []);

  return (
    <iframe
      title="Harness Console"
      src={`/console-v2/index.html?${params}`}
      style={{ display: "block", width: "100vw", height: "100vh", border: 0 }}
    />
  );
}

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <Routes>
        <Route path="/" element={<Console screen="home" />} />
        <Route path="/dashboard" element={<Console screen="home" />} />
        <Route path="/overview" element={<Console screen="home" />} />
        <Route path="/worktrees" element={<Console screen="worktrees" />} />
        <Route path="/usage" element={<Console screen="usage" />} />
      </Routes>
      <TokenPrompt />
    </QueryClientProvider>
  );
}
