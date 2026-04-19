import { useEffect } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Route, Routes, useLocation } from "react-router-dom";
import { Dashboard } from "./routes/Dashboard";
import { Overview } from "./routes/Overview";
import { PaletteProvider } from "./lib/palette";
import { TokenPrompt } from "./components/TokenPrompt";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      refetchInterval: 5000,
      refetchIntervalInBackground: true,
      retry: 3,
      staleTime: 0,
    },
  },
});

/**
 * Scroll to a fragment target when the URL hash changes. React Router v6
 * updates the URL for hash links but does NOT scroll — so sidebar items
 * like /overview#projects would otherwise update the bar with no visible
 * effect. Runs after the route commits so the panel being targeted has
 * already mounted.
 */
function ScrollToHash() {
  const { hash, pathname } = useLocation();
  useEffect(() => {
    if (!hash) return;
    const id = hash.slice(1);
    // Defer to after paint so newly-mounted panels exist in the DOM.
    const t = setTimeout(() => {
      const el = document.getElementById(id);
      if (el) el.scrollIntoView({ behavior: "smooth", block: "start" });
    }, 0);
    return () => clearTimeout(t);
  }, [hash, pathname]);
  return null;
}

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <PaletteProvider>
        <ScrollToHash />
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/overview" element={<Overview />} />
        </Routes>
        <TokenPrompt />
      </PaletteProvider>
    </QueryClientProvider>
  );
}
