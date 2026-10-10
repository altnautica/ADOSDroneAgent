import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import { consumeUrlKey } from "./shared/api-key";
import { probePairingInfo } from "./shared/use-profile";
import { configureResource } from "./shared/use-resource";
import { pollIntervalFor, useReachStore } from "./stores/reach-store";
import "./styles/globals.css";

// Capture a one-shot ?ados_key=… URL parameter into storage before any render
// (off-box / tunnel access). On-box the panel is trusted and this is a no-op.
consumeUrlKey();

// Every screen poll reports refusals to the reach store and backs off while
// the node is refusing (the answer cannot change until an operator acts).
configureResource({
  report: (status, ok) => useReachStore.getState().report(status, ok),
  intervalFor: (base) => pollIntervalFor(base, useReachStore.getState().refusal),
});

// The public identity route answers even when everything else is refused, so
// it is where the operator's way out (the pairing code) comes from.
void probePairingInfo().then((info) =>
  useReachStore.getState().setPairingCode(info.pairing_code ?? null),
);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
