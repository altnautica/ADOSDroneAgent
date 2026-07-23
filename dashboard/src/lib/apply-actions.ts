// Wrappers around the agent's batch apply endpoint. Each settings
// section sends its own slice; the agent rolls back on first failure.
//
// /api/v1/setup/apply accepts ApplyRequest with optional sections:
// profile, network, cloud, display, advanced. Response is per-section
// ok/message + an `overall` flag and the list of sections that were
// rolled back when a later section failed.

import { z } from "zod";

import { apiFetch } from "./api";
import type { GroundRole, Profile } from "./types";

// ---- request shapes ------------------------------------------------

export const profileSectionSchema = z.object({
  profile: z.enum(["drone", "ground_station"]),
  ground_role: z.enum(["direct", "relay", "receiver"]).optional(),
  auto_restart: z.boolean().optional(),
});

export const networkSectionSchema = z.object({
  wifi_ssid: z.string().min(1).max(32).optional(),
  wifi_password: z.string().min(8).max(63).optional(),
  hotspot_enabled: z.boolean().optional(),
});

export const cloudSelfHostedSchema = z.object({
  url: z.string().url(),
  mqtt_broker: z.string().min(1),
  mqtt_port: z.number().int().min(1).max(65535),
  api_key: z.string().optional(),
});

export const cloudSectionSchema = z.object({
  mode: z.enum(["cloud", "self_hosted", "local"]),
  self_hosted: cloudSelfHostedSchema.optional(),
});

export const advancedSectionSchema = z.object({
  board_override: z
    .string()
    .max(64)
    .regex(/^[A-Za-z0-9_-]*$/, "Letters, digits, dash and underscore only.")
    .optional(),
  log_level: z.enum(["debug", "info", "warning", "error", "critical"]).optional(),
});

export type ProfileSection = z.infer<typeof profileSectionSchema>;
export type NetworkSection = z.infer<typeof networkSectionSchema>;
export type CloudSection = z.infer<typeof cloudSectionSchema>;
export type AdvancedSection = z.infer<typeof advancedSectionSchema>;

export interface ApplyPayload {
  profile?: ProfileSection;
  network?: NetworkSection;
  cloud?: CloudSection;
  advanced?: AdvancedSection;
}

// ---- response shapes ----------------------------------------------

export interface ApplyResultSection {
  ok: boolean;
  message: string;
  data?: Record<string, unknown>;
}

export interface ApplyResponse {
  overall: boolean;
  sections: Record<string, ApplyResultSection>;
  rolled_back: string[];
}

export function postApply(payload: ApplyPayload) {
  return apiFetch<ApplyResponse>("/api/v1/setup/apply", {
    method: "POST",
    body: payload,
  });
}

// The PUT /api/config response. The handler returns HTTP 200 even for soft
// failures: an unknown key or a bad value comes back as `{error}`, and a failed
// disk write as `{persisted: false, persist_error}`. Curated pages must inspect
// these, not just the HTTP status.
export interface ConfigWriteResult {
  status?: string;
  key?: string;
  value?: unknown;
  persisted?: boolean;
  persist_error?: string;
  error?: string;
  ok?: boolean;
}

// A single dot-path config write (the same PUT /api/config the GCS uses). Used
// by the Offload settings page, whose keys are additive perception.* fields with
// no batch-apply section — a per-key write is simpler and mirrors the GCS.
export function putConfigValue(key: string, value: string) {
  return apiFetch<ConfigWriteResult>("/api/config", {
    method: "PUT",
    body: { key, value },
  });
}

// putConfigValue that surfaces the agent's soft-error bodies as real failures so
// a page can toast + roll back instead of silently swallowing them. The write
// is a string; the agent coerces it to the leaf's live type (a bool key accepts
// "true"/"false", an int key a numeric string, etc.).
export async function putConfigChecked(
  key: string,
  value: string,
): Promise<ConfigWriteResult> {
  const res = await putConfigValue(key, value);
  if (res.error) throw new Error(res.error);
  if (res.persisted === false) {
    throw new Error(res.persist_error || "The change was not saved to disk.");
  }
  return res;
}

// Helpers used by section pages to read suggested defaults from the
// existing setup status payload.

export function profileFromStatus(
  profile: Profile | undefined,
  fallback: "drone" | "ground_station" = "drone",
): "drone" | "ground_station" {
  if (!profile || profile === "auto" || profile === "unknown") return fallback;
  return profile;
}

export function groundRoleFromStatus(
  role: GroundRole | undefined,
  fallback: GroundRole = "direct",
): GroundRole {
  return role ?? fallback;
}
