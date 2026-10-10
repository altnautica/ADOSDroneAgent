// The live vehicle state the Feed reads: attitude, position, battery, GPS and
// the freshness/provenance verdicts. Written by the Feed's 5 Hz poll
// (`useFlightTelemetryPoll`); every instrument subscribes with a field
// selector so a poll re-renders only the instruments whose inputs changed.

import { create } from "zustand";

import type { VehicleState } from "@/lib/types";

export interface HomePoint {
  lat: number;
  lon: number;
}

export interface FlightState {
  telemetry: VehicleState | null;
  /** True when the most recent poll failed (the snapshot may be old). */
  stale: boolean;
  /** Fresh attitude is present, whatever its source. Gates the instruments. */
  live: boolean;
  /** The readings came over the radio from another node. */
  relayed: boolean;
  /** Where the vehicle was when it last armed: the autopilot sets home at
   *  arming, and the vehicle snapshot carries no home position of its own.
   *  Null until an arming transition has been seen with a position fix. */
  home: HomePoint | null;
  /** `performance.now()` of the last poll that carried live telemetry, or null
   *  when the vehicle has not been live this session. */
  lastLiveAt: number | null;
}

export const INITIAL_FLIGHT_STATE: FlightState = {
  telemetry: null,
  stale: false,
  live: false,
  relayed: false,
  home: null,
  lastLiveAt: null,
};

export const useFlightStore = create<FlightState>(() => ({ ...INITIAL_FLIGHT_STATE }));
