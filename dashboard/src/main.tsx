import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter } from "react-router-dom";

import { App } from "./App";
import { consumeUrlKey } from "./lib/api-key";
import "./styles/globals.css";

// Capture a one-shot ?ados_key=… URL parameter into localStorage before
// any React render. Used by tunnel links so the dashboard can authenticate
// across a cross-origin boundary.
consumeUrlKey();

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // 5s staleTime keeps panels stable across remounts and tab focus
      // events while leaving room for explicit polling intervals at the
      // hook level for hot data (status / snapshot / heartbeat).
      staleTime: 5_000,
      refetchOnWindowFocus: false,
      retry: 1,
    },
  },
});

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <App />
      </BrowserRouter>
    </QueryClientProvider>
  </StrictMode>,
);
