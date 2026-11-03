// The Cloud settings page: the posture and self-hosted endpoint draft, its
// dirty rule, validation and payload, plus the tunnel token request.
//
// The posture comes from setup status `cloud_choice.mode`; the self-hosted
// URL, broker and port are seeded from the config so re-applying never blanks
// the broker or resets the port. The API key is write-only: it starts empty
// and an empty key keeps the stored one.

import { cloudSectionSchema, type CloudSection } from "@/shared/api/apply-actions";
import { selfHostedSeed } from "@/shared/api/cloud-seed";
import type { AgentConfig } from "@/shared/api/types";

export type CloudMode = CloudSection["mode"];

export const CLOUD_MODE_OPTIONS = [
  { value: "local", labelKey: "settingsMore.cloud.modeLocal", descriptionKey: "settingsMore.cloud.modeLocalDesc" },
  { value: "cloud", labelKey: "settingsMore.cloud.modeCloud", descriptionKey: "settingsMore.cloud.modeCloudDesc" },
  {
    value: "self_hosted",
    labelKey: "settingsMore.cloud.modeSelfHosted",
    descriptionKey: "settingsMore.cloud.modeSelfHostedDesc",
  },
] as const satisfies readonly { value: CloudMode; labelKey: string; descriptionKey: string }[];

export interface CloudDraft {
  mode: CloudMode;
  url: string;
  broker: string;
  /** As typed. */
  port: string;
  apiKey: string;
}

export function cloudModeFrom(raw: unknown): CloudMode {
  return raw === "cloud" || raw === "self_hosted" ? raw : "local";
}

export function cloudSeed(mode: CloudMode, config: AgentConfig | null | undefined): CloudDraft {
  const seed = selfHostedSeed(config ?? undefined);
  return { mode, url: seed.url, broker: seed.broker, port: String(seed.port), apiKey: "" };
}

/** Endpoint edits only count while self-hosted is chosen. */
export function cloudDirty(draft: CloudDraft, initial: CloudDraft): boolean {
  if (draft.mode !== initial.mode) return true;
  return (
    draft.mode === "self_hosted" &&
    (draft.url !== initial.url ||
      draft.broker !== initial.broker ||
      draft.port !== initial.port ||
      draft.apiKey !== "")
  );
}

export function cloudSection(draft: CloudDraft): CloudSection {
  if (draft.mode !== "self_hosted") return { mode: draft.mode };
  return {
    mode: draft.mode,
    self_hosted: {
      url: draft.url,
      mqtt_broker: draft.broker,
      mqtt_port: Number(draft.port),
      api_key: draft.apiKey || undefined,
    },
  };
}

/** The section to send, or the first validation issue as `path: message`. */
export function validateCloud(draft: CloudDraft): { ok: true; section: CloudSection } | { ok: false; error: string } {
  const section = cloudSection(draft);
  const result = cloudSectionSchema.safeParse(section);
  if (result.success) return { ok: true, section };
  const first = result.error.issues[0];
  return { ok: false, error: first ? `${first.path.join(".")}: ${first.message}` : "invalid" };
}

/** The tunnel token body, or null when nothing was entered. */
export function tunnelTokenBody(token: string): { token_or_script: string } | null {
  const trimmed = token.trim();
  return trimmed === "" ? null : { token_or_script: trimmed };
}
