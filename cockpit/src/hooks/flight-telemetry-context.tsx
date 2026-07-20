// A single shared flight-telemetry poll for the Feed. Provided at the Feed
// screen so the artificial horizon, both tapes, and the telemetry strip read
// one 5 Hz poll (not one per instrument), and so the poll runs only while the
// Feed is on screen (it stops the moment the pilot leaves the flying view).

import { createContext, useContext, type ReactNode } from "react";

import {
  useFlightTelemetry,
  type FlightTelemetryState,
} from "@/hooks/use-flight-telemetry";

const FlightTelemetryContext = createContext<FlightTelemetryState>({
  telemetry: null,
  stale: false,
  live: false,
});

export function FlightTelemetryProvider({ children }: { children: ReactNode }) {
  const value = useFlightTelemetry();
  return (
    <FlightTelemetryContext.Provider value={value}>
      {children}
    </FlightTelemetryContext.Provider>
  );
}

export function useFlightTelemetryContext(): FlightTelemetryState {
  return useContext(FlightTelemetryContext);
}
