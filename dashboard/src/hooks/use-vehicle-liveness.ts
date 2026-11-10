// Vehicle liveness for the Home status card, by the cockpit's rule
// (`@/shared/hud/vehicle-live`): live while `/api/telemetry` carries usable
// attitude whose vehicle stamp keeps moving. An answered read is not enough:
// with the flight controller down the agent still answers, with the vehicle
// fields stripped. Each new read is folded in once, keyed by its receipt time.

import { useState } from "react";

import { isLive as readIsLive } from "@/lib/freshness";
import { INITIAL_LIVENESS, msSinceLive, nextLiveness, type LiveSample, type Liveness } from "@/shared/hud/vehicle-live";

export interface VehicleRead {
  data: LiveSample | null | undefined;
  dataUpdatedAt: number;
  isError: boolean;
}

export interface VehicleLiveness {
  /** The last read is current and its sample live. */
  live: boolean;
  /** Ms since the vehicle was last live; null when never live this session. */
  msSinceLive: number | null;
  /** The arm state at the last live sample; null before any. */
  armedAtLastLive: boolean | null;
}

/** The liveness view of folded samples at `nowMs`; `readLive` says the read
 *  that brought the last sample is still current. */
export function vehicleLiveness(l: Liveness, nowMs: number, readLive: boolean): VehicleLiveness {
  const since = msSinceLive(l, nowMs, readLive);
  return { live: since === 0, msSinceLive: since, armedAtLastLive: l.armedAtLastLive };
}

/** Liveness of the telemetry read `tel`, polled every `pollMs`, at `nowMs`. */
export function useVehicleLiveness(tel: VehicleRead, pollMs: number, nowMs: number): VehicleLiveness {
  const [state, setState] = useState(INITIAL_LIVENESS);
  let l = state;
  if (tel.dataUpdatedAt > 0 && tel.dataUpdatedAt !== state.sampleAt) {
    // A new read, folded in while rendering.
    l = nextLiveness(state, tel.data, tel.dataUpdatedAt);
    setState(l);
  }
  return vehicleLiveness(l, nowMs, readIsLive(tel, pollMs, nowMs));
}
