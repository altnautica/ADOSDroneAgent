import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createBrowserRouter, RouterProvider } from "react-router-dom";

import { App } from "./App";
import { consumeUrlKey } from "./shared/api-key";
import "@/shared/i18n";
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
      // Polls stop while the tab is hidden; they resume on return.
      refetchIntervalInBackground: false,
      retry: 1,
    },
  },
});

// A data router so pages can block in-app navigation while a form is dirty
// (`useBlocker`); the app's own <Routes> tree sits under one splat route.
const router = createBrowserRouter([{ path: "*", element: <App /> }]);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
);
