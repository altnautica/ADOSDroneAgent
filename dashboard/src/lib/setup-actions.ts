// Wrappers around the config-write /api/v1/setup/* endpoints used by
// the post-install dashboard (Region settings, the hardware panel, and
// the diagnostics reboot action). Onboarding itself lives in the CLI
// installer, not the browser.

import { apiFetch } from "./api";
import type { RegulatoryMode } from "./types";

// Operating-region (RF regulatory posture). Persists the operator's
// choice via the batch apply endpoint so it rolls back on failure and
// reports restart_required when the radio must re-read at restart.
// region is required when mode === "region" (uppercase ISO alpha-2).
export interface RegionPayload {
  mode: RegulatoryMode;
  region?: string | null;
}

export interface RegionApplySection {
  ok: boolean;
  message: string;
  data?: { restart_required?: boolean } & Record<string, unknown>;
}

export interface RegionApplyResponse {
  overall: boolean;
  sections: Record<string, RegionApplySection>;
  rolled_back: string[];
}

export function postRegion(payload: RegionPayload) {
  return apiFetch<RegionApplyResponse>("/api/v1/setup/apply", {
    method: "POST",
    body: { regulatory: payload },
  });
}

export function refreshHardwareCheck() {
  return apiFetch("/api/v1/setup/hardware-check/refresh", { method: "POST" });
}

export function rebootAgent() {
  return apiFetch("/api/v1/setup/reboot", { method: "POST" });
}
