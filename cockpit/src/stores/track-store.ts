// The vehicle's recent ground track — a bounded breadcrumb the minimap draws.
// Fed one fix at a time from the Feed's flight-telemetry poll; a per-fix jitter
// filter drops sub-metre GPS noise so the trail is real movement, and the ring
// is capped so a long flight cannot grow memory without bound.
//
// `start` is the FIRST valid fix seen this session — the map's best proxy for the
// launch point and honestly labelled "start", NOT the flight controller's home
// datum (the telemetry snapshot carries no home). It is captured once and never
// fabricated: no valid fix, no start marker.

import { create } from "zustand";

import { distanceMeters, isValidFix } from "@/lib/minimap-geometry";

/** Ignore a new fix closer than this to the last kept one — GPS jitter, not
 *  movement. */
const MIN_MOVE_M = 1.5;
/** Cap the breadcrumb ring (a long flight cannot grow memory without bound). */
const MAX_SAMPLES = 480;

export interface TrackSample {
  lat: number;
  lon: number;
  /** Epoch ms the fix was recorded. */
  t: number;
}

interface TrackState {
  /** The first valid fix this session (the "start"/launch proxy), or null. */
  start: TrackSample | null;
  /** The bounded breadcrumb ring, oldest first. */
  samples: TrackSample[];
  /** The vehicle this track belongs to — the last non-null paired device_id
   *  seen. Null until a vehicle is identified. Used only to detect a change of
   *  vehicle; not drawn. */
  vehicleId: string | null;
  /** Record a fix. Invalid fixes are dropped; a fix within MIN_MOVE_M of the last
   *  kept sample is dropped as jitter; the first valid fix sets `start`. */
  record: (lat: number, lon: number) => void;
  /** Drop the whole track (new vehicle / feed reset). */
  clear: () => void;
  /** Reconcile the track against the currently-paired vehicle identity. A change
   *  to a DIFFERENT vehicle drops the breadcrumb + start, so the minimap never
   *  draws a cross-vehicle trail or mislabels one vehicle's first fix as
   *  another's session start (Rule 44) — the failure a mid-session re-pair on a
   *  ground-station cockpit would otherwise cause. A null id (a transient link
   *  drop / unpaired) is ignored so a brief dropout does not wipe a real track,
   *  and the first identity seen adopts the in-progress track rather than
   *  clearing it (a drone records its own fixes before the radio binds; its page
   *  reload already resets a genuine restart). */
  syncVehicle: (deviceId: string | null) => void;
}

export const useTrackStore = create<TrackState>((set, get) => ({
  start: null,
  samples: [],
  vehicleId: null,
  record: (lat, lon) => {
    if (!isValidFix(lat, lon)) return;
    const { start, samples } = get();
    const last = samples[samples.length - 1];
    if (last && distanceMeters(last, { lat, lon }) < MIN_MOVE_M) return;
    const sample: TrackSample = { lat, lon, t: Date.now() };
    const next = [...samples, sample];
    // Bound the ring: drop the oldest once past the cap.
    if (next.length > MAX_SAMPLES) next.splice(0, next.length - MAX_SAMPLES);
    set({ samples: next, start: start ?? sample });
  },
  clear: () => set({ start: null, samples: [] }),
  syncVehicle: (deviceId) => {
    if (deviceId == null) return;
    const { vehicleId, clear } = get();
    if (vehicleId != null && vehicleId !== deviceId) clear();
    set({ vehicleId: deviceId });
  },
}));
