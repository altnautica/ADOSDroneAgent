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

export function skipSetup() {
  return apiFetch("/api/v1/setup/skip", { method: "POST" });
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

export interface NavigationConfigPayload {
  mode: "off" | "optical-flow" | "vio" | "both";
  rangefinder?: {
    topology: "companion" | "fc";
    driver: "tfluna_uart" | "garmin_lidarlite_i2c" | "vl53l1x_i2c";
    device: { path: string; baud?: number; address?: string };
  };
  plugin_id?: string;
}

export function postNavigationConfig(payload: NavigationConfigPayload) {
  return apiFetch<SetupActionResult>("/api/v1/setup/navigation/config", {
    method: "POST",
    body: payload,
  });
}

export interface NavigationAssignCameraPayload {
  device_path: string;
  role: "nav" | "secondary" | "thermal" | "inspection" | "primary";
}

export interface RoleConflictDetail {
  error: "role_conflict";
  device_path: string;
  current_role: string;
  current_plugin: string;
  requested_role: string;
  message: string;
}

export function isRoleConflictDetail(value: unknown): value is RoleConflictDetail {
  if (!value || typeof value !== "object") return false;
  const v = value as { error?: unknown };
  return v.error === "role_conflict";
}

export function postNavigationAssignCamera(
  payload: NavigationAssignCameraPayload,
  opts: { force?: boolean } = {},
) {
  const qs = opts.force ? "?force=true" : "";
  return apiFetch<SetupActionResult>(
    `/api/v1/setup/navigation/assign-camera${qs}`,
    {
      method: "POST",
      body: payload,
    },
  );
}
