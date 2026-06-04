// Live WFB-tx / WFB-rx runtime read. The agent exposes the same
// /api/wfb endpoint on both profiles; the response carries TX-side
// and RX-side fields together and the caller decides which to render
// based on the agent profile and the reported state.

import { apiFetch } from "./api";
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
