import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import { consumeUrlKey } from "./lib/api-key";
import "./styles/globals.css";

// Capture a one-shot ?ados_key=… URL parameter into storage before any render
// (off-box / tunnel access). On-box the panel is trusted and this is a no-op.
consumeUrlKey();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
