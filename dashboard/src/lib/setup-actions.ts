// Wrappers around the existing /api/v1/setup/* endpoints. The wizard
// calls one of these for each step's POST.

import { apiFetch } from "./api";
import type { GroundRole, Profile } from "./types";

export interface SetupActionResult {
  ok: boolean;
  message?: string;
  detail?: unknown;
}

export interface ProfilePayload {
  profile: Profile;
  ground_role?: GroundRole;
  source?: "user" | "detected" | "auto";
}

export function postProfile(payload: ProfilePayload) {
  return apiFetch<SetupActionResult>("/api/v1/setup/profile", {
    method: "POST",
    body: payload,
  });
}

export interface CloudChoicePayload {
  mode: "cloud" | "self_hosted" | "local";
  backend_url?: string;
  mqtt_broker?: string;
  mqtt_port?: number;
  api_key?: string;
}

export function postCloudChoice(payload: CloudChoicePayload) {
  return apiFetch<SetupActionResult>("/api/v1/setup/cloud-choice", {
    method: "POST",
    body: payload,
  });
}

export function refreshHardwareCheck() {
  return apiFetch("/api/v1/setup/hardware-check/refresh", { method: "POST" });
}

export function finishSetup() {
  return apiFetch("/api/v1/setup/finish", { method: "POST" });
}

export function skipStep(stepId: string) {
  return apiFetch(`/api/v1/setup/step/${encodeURIComponent(stepId)}/skip`, {
    method: "POST",
  });
}

export function installCloudflared(payload: { token?: string; quick?: boolean }) {
  return apiFetch("/api/v1/setup/remote-access/cloudflare", {
    method: "POST",
    body: payload,
  });
}

export function rebootAgent() {
  return apiFetch("/api/v1/setup/reboot", { method: "POST" });
}
