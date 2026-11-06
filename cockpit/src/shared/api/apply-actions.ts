// Wrappers around the agent's batch apply endpoint. Each settings
// section sends its own slice; the agent rolls back on first failure.
//
// /api/v1/setup/apply accepts ApplyRequest with optional sections:
// profile, network, cloud, display, advanced. Response is per-section
// ok/message + an `overall` flag and the list of sections that were
// rolled back when a later section failed.

import { z } from "zod";

import { apiFetch } from "../api-fetch";
import type { Profile } from "./types";

// ---- request shapes ------------------------------------------------

export const profileSectionSchema = z.object({
  profile: z.enum(["drone", "ground_station", "visor"]),
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
  /** Override the armed interlock after the operator confirmed. */
  force?: boolean;
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

// A single dot-path config write (the same PUT /api/config the GCS uses) that
// surfaces the agent's soft-error bodies as real failures, so a page can toast
// and roll back instead of reporting a refused write as saved. A string is
// coerced by the agent to the leaf's live type (a bool key accepts
// "true"/"false", an int key a numeric string, etc.); a number is taken as
// sent, which is how a key whose current value is null (an optional field not
// yet set) gets a numeric value. `force` overrides the agent's armed interlock
// on link keys, sent only after the operator confirmed.
export async function putConfigChecked(
  key: string,
  value: string | number,
  opts: { force?: boolean } = {},
): Promise<ConfigWriteResult> {
  const res = await apiFetch<ConfigWriteResult>("/api/config", {
    method: "PUT",
    body: opts.force ? { key, value, force: true } : { key, value },
  });
  if (res.error) throw new Error(res.error);
  if (res.persisted === false) {
    throw new Error(res.persist_error || "The change was not saved to disk.");
  }
  return res;
}

/** A profile the apply form can submit. */
export type ApplyProfile = ProfileSection["profile"];

// Narrows a resolved profile to one the apply form can submit.
export function profileFromStatus(
  profile: Profile | undefined,
  fallback: ApplyProfile = "drone",
): ApplyProfile {
  if (profile === "drone" || profile === "ground_station" || profile === "visor") return profile;
  return fallback;
}

/** A confirm line of a profile or role change. */
export type ProfileChangeNoteKey =
  | "settings.profile.confirmRoleRestart"
  | "settings.profile.confirmSwitch"
  | "settings.profile.confirmKioskStops"
  | "settings.profile.confirmAccessPointStops";

/**
 * The confirm lines for applying `target` on a node now running `current`:
 * what the change does to the node's services. A profile change moves the
 * node onto the new profile's services (the agent stops and disables the old
 * profile's, then restarts), so the HDMI kiosk goes with a ground station and
 * the Wi-Fi access point with a ground station or visor that becomes a drone.
 * A role change on the same profile is a plain restart.
 */
export function profileChangeNoteKeys(current: ApplyProfile, target: ApplyProfile): ProfileChangeNoteKey[] {
  if (current === target) return ["settings.profile.confirmRoleRestart"];
  const keys: ProfileChangeNoteKey[] = ["settings.profile.confirmSwitch"];
  if (current === "ground_station") keys.push("settings.profile.confirmKioskStops");
  if (current !== "drone" && target === "drone") keys.push("settings.profile.confirmAccessPointStops");
  return keys;
}
