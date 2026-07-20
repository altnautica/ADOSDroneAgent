// A single shared telemetry poll for the whole shell. The shell provides it;
// the status strip, the Feed HUD, and any screen that wants live status read
// it — so the panel polls the agent once, not once per consumer.

import { createContext, useContext, type ReactNode } from "react";

import { useTelemetry, type TelemetryState } from "@/hooks/use-telemetry";

const TelemetryContext = createContext<TelemetryState>({
  status: null,
  error: null,
  stale: false,
});

export function TelemetryProvider({ children }: { children: ReactNode }) {
  const telemetry = useTelemetry();
  return (
    <TelemetryContext.Provider value={telemetry}>
      {children}
    </TelemetryContext.Provider>
  );
}

export function useTelemetryContext(): TelemetryState {
  return useContext(TelemetryContext);
}
