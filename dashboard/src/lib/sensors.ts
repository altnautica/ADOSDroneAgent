// Wire types for `/api/v1/{sensors,clock,storage,gnss}` and the pure helpers
// the sensor, GNSS and capture-settings pages render from.

import { apiFetch } from "@/shared/api-fetch";

export interface SensorRow {
  id: string;
  kind: string;
  role: string;
  source: string;
  rate_hz: number;
  expected_hz: number | null;
  last_t_ns: number;
  age_ms: number | null;
  sigma_ns: number;
}

export interface SensorsStatus {
  present: boolean;
  rig_id: string | null;
  calib_version: string | null;
  hub: {
    connected: boolean;
    transport: string;
    fw: string | null;
    board: string | null;
    proto_ver: number | null;
    uptime_s: number | null;
    crc_errors: number;
    dropped: number;
  } | null;
  fc: { connected: boolean; autopilot: string | null; highres_imu: boolean } | null;
  sensors: SensorRow[];
  gnss: { fix: string; sats: number; hdop: number | null; rtk: string } | null;
}

export interface ClockSource {
  id: string;
  kind: string;
  locked: boolean;
  offset_ns: number;
  skew_ppm: number;
  sigma_ns: number;
  last_update_ns: number;
}

export interface ClockStatus {
  present: boolean;
  timebase: string | null;
  locked: boolean;
  sigma_ns: number;
  t_ns: number;
  utc_ns: number | null;
  sources: ClockSource[];
}

export interface StorageStatus {
  present: boolean;
  capture_root: string | null;
  volumes: {
    mount: string;
    device: string;
    fs: string;
    total_bytes: number;
    free_bytes: number;
    rotational: boolean;
    is_capture: boolean;
  }[];
  capture: {
    used_bytes: number;
    budget_bytes: number;
    budget_pct: number;
    floor_free_bytes: number;
    sessions: number;
    uploaded: number;
    evict_unuploaded: boolean;
    state: "ok" | "near_full" | "full";
  } | null;
  write_probe: { mbps: number; measured_at_ns: number } | null;
}

export interface GnssBase {
  mode: "off" | "survey" | "fixed";
  state: "off" | "surveying" | "fixed" | "failed";
  elapsed_s: number;
  min_dur_s: number;
  accuracy_m: number | null;
  target_accuracy_m: number | null;
  fixed: { lat_deg: number; lon_deg: number; h_ellipsoid_m: number } | null;
  rtcm_out: { msgs_per_s: number; bytes_total: number; targets: number };
}

export interface GnssStatus {
  present: boolean;
  receivers: {
    id: string;
    source: string;
    model: string | null;
    fix: string;
    sats: number;
    hdop: number | null;
    lat_deg: number | null;
    lon_deg: number | null;
    h_ellipsoid_m: number | null;
    h_msl_m: number | null;
    rtk: string;
    pps_locked: boolean;
  }[];
  base: GnssBase | null;
  heading: { available: boolean; deg: number | null; accuracy_deg: number | null; baseline_m: number | null } | null;
}

export const SENSORS_PATH = "/api/v1/sensors";
export const CLOCK_PATH = "/api/v1/clock";
export const STORAGE_PATH = "/api/v1/storage";
export const GNSS_PATH = "/api/v1/gnss";

/** A sensor stream is `ok` within 10 % of its expected rate, `low` below 50 %
 *  of it, `degraded` in between; `none` when no expected rate is known. */
export type RateVerdict = "ok" | "degraded" | "low" | "none";

export function rateVerdict(row: Pick<SensorRow, "rate_hz" | "expected_hz">): RateVerdict {
  if (row.expected_hz == null || row.expected_hz <= 0) return "none";
  const ratio = row.rate_hz / row.expected_hz;
  if (ratio >= 0.9) return "ok";
  if (ratio >= 0.5) return "degraded";
  return "low";
}

/** A σ in ns as a readable figure; the wire uses -1 for unknown. */
export function formatSigma(sigmaNs: number): string {
  if (sigmaNs < 0) return "—";
  if (sigmaNs >= 1_000_000) return `${(sigmaNs / 1_000_000).toFixed(2)} ms`;
  return `${(sigmaNs / 1000).toFixed(0)} µs`;
}

export function putGnssBase(
  body: { mode: "off" } | { mode: "survey"; min_dur_s: number; accuracy_m: number },
): Promise<GnssBase> {
  return apiFetch<GnssBase>("/api/v1/gnss/base", { method: "PUT", body });
}

/** Upload a contract `rig.json`. Invalid documents answer 400 `E_RIG_INVALID`. */
export function putRig(rig: unknown): Promise<{ ok: boolean; rig_id: string; calib_version: string }> {
  return apiFetch("/api/v1/sensors/rig", { method: "PUT", body: rig });
}

/** Parse a picked rig file before upload; returns the document, or why it is
 *  not one (`parse` carries the JSON error text). */
export function parseRigFile(
  text: string,
): { ok: true; rig: Record<string, unknown> } | { ok: false; reason: "parse" | "shape"; detail: string } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (err) {
    return { ok: false, reason: "parse", detail: err instanceof Error ? err.message : String(err) };
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    return { ok: false, reason: "shape", detail: "" };
  }
  return { ok: true, rig: parsed as Record<string, unknown> };
}
