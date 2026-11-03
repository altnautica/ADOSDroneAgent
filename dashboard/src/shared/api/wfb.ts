// Live WFB-tx / WFB-rx runtime read. The agent exposes the same
// /api/wfb endpoint on both profiles; the response carries TX-side
// and RX-side fields together and the caller decides which to render
// based on the agent profile and the reported state.

import { apiFetch } from "../api-fetch";
import type { WfbStatus } from "./types";

export function getWfbStatus() {
  return apiFetch<WfbStatus>("/api/wfb");
}

/** Radio link-tuning knobs the agent applies to the live data plane and
 *  persists to config. Every field is optional; the agent applies the ones
 *  present and returns a snapshot plus a `warnings` array for partial applies. */
export interface VideoConfigPatch {
  fec_k?: number;
  fec_n?: number;
  mcs?: number;
  preset?: "conservative" | "balanced" | "aggressive";
  auto?: boolean;
}

export interface VideoConfigResult {
  warnings?: string[];
  [key: string]: unknown;
}

export function setVideoConfig(patch: VideoConfigPatch) {
  return apiFetch<VideoConfigResult>("/api/video/config", {
    method: "POST",
    body: patch,
  });
}

/** The agent's answer to a TX power write: the requested value, what the
 *  radio applied, the region ceiling it was bounded by, and whether it was
 *  saved to config (`persist_error` when applied but not saved). */
export interface TxPowerResult {
  requested_dbm: number;
  effective_dbm: number;
  tx_power_max_dbm: number;
  persisted: boolean;
  persist_error?: string;
}

/** Set the live WFB TX power (dBm). The agent refuses below 1 dBm or above
 *  `video.wfb.tx_power_max_dbm` (400) and while the vehicle is armed (409
 *  `E_ARMED`), and persists the value only when the radio applied it. */
export function setWfbTxPower(dbm: number) {
  return apiFetch<TxPowerResult>("/api/wfb/tx-power", {
    method: "PUT",
    body: { tx_power_dbm: dbm },
  });
}
