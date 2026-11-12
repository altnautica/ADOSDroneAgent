// The GNSS screen's base-mode write. Changing the base mode restarts the
// survey or stops the RTCM correction stream the rovers depend on, so both
// directions go out through a guarded button; this module builds the request
// and says why it cannot go.

import type { GnssBase } from "@/shared/api/sensors";

export type BaseRequest =
  | { mode: "off" }
  | { mode: "survey"; min_dur_s: number; accuracy_m: number }
  | { mode: "fixed"; lat_deg: number; lon_deg: number; h_ellipsoid_m: number };

/** A fixed-base request from the first receiver with a full 3D position, or
 *  null when none has one (a fixed base needs a known point). */
export function fixedRequest(
  receivers: readonly { fix: string; lat_deg: number | null; lon_deg: number | null; h_ellipsoid_m: number | null }[],
): BaseRequest | null {
  const r = receivers.find(
    (x) => x.fix !== "none" && x.fix !== "2d" && x.lat_deg != null && x.lon_deg != null && x.h_ellipsoid_m != null,
  );
  if (!r || r.lat_deg == null || r.lon_deg == null || r.h_ellipsoid_m == null) return null;
  return { mode: "fixed", lat_deg: r.lat_deg, lon_deg: r.lon_deg, h_ellipsoid_m: r.h_ellipsoid_m };
}

/** A survey request from the operator's entries, or null when either is not a
 *  positive number (minimum duration a whole number of seconds). */
export function surveyRequest(minDur: string, accuracy: string): BaseRequest | null {
  const d = Number(minDur.trim());
  const a = Number(accuracy.trim());
  if (!minDur.trim() || !accuracy.trim()) return null;
  if (!Number.isInteger(d) || d <= 0 || !Number.isFinite(a) || a <= 0) return null;
  return { mode: "survey", min_dur_s: d, accuracy_m: a };
}

/** Whether the base can be turned off: only when it is not already off. */
export function canTurnBaseOff(base: GnssBase | null | undefined, busy: boolean): boolean {
  return !busy && !!base && base.mode !== "off";
}

/** The survey's progress 0..1 by elapsed time against the minimum duration;
 *  null outside a survey. */
export function surveyFraction(base: GnssBase | null | undefined): number | null {
  if (!base || base.state !== "surveying" || base.min_dur_s <= 0) return null;
  return Math.min(1, Math.max(0, base.elapsed_s / base.min_dur_s));
}
