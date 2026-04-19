import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Route, Routes } from "react-router-dom";
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

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <PaletteProvider>
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/overview" element={<Overview />} />
        </Routes>
        <TokenPrompt />
      </PaletteProvider>
    </QueryClientProvider>
  );
}
