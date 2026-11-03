// The Region settings page: the regulatory mode and region draft, seeded from
// `network.regulatory` in the config, its validity, dirty rule and payload.

import { COMMON_REGIONS, modeFromConfig, normalizeRegion, regionFromConfig } from "@/shared/api/region";
import type { RegionPayload } from "@/shared/api/setup-actions";
import type { RegulatoryInfo, RegulatoryMode } from "@/shared/api/types";

/** The choice that reveals the free ISO code field. */
export const OTHER_REGION = "__other__";

export interface RegionDraft {
  mode: RegulatoryMode;
  /** A common region code or `OTHER_REGION`. */
  choice: string;
  otherCode: string;
}

export interface RegionCurrent {
  mode: RegulatoryMode;
  region: string | null;
}

/** The agent's posture. An absent block or mode is unrestricted (a fresh box). */
export function regionCurrent(reg: RegulatoryInfo | undefined | null): RegionCurrent {
  return { mode: modeFromConfig(reg ?? undefined), region: regionFromConfig(reg ?? undefined) };
}

/** A draft seeded from the agent's posture: a pinned region that is not a
 *  common one lands in the free code field. */
export function regionSeed(current: RegionCurrent): RegionDraft {
  const region = current.region;
  if (region == null) return { mode: current.mode, choice: COMMON_REGIONS[0].code, otherCode: "" };
  if (COMMON_REGIONS.some((c) => c.code === region)) return { mode: current.mode, choice: region, otherCode: "" };
  return { mode: current.mode, choice: OTHER_REGION, otherCode: region };
}

/** The region the draft names, or null when the free code is not valid. */
export function draftRegion(draft: RegionDraft): string | null {
  return draft.choice === OTHER_REGION ? normalizeRegion(draft.otherCode) : draft.choice;
}

export function regionValid(draft: RegionDraft): boolean {
  return draft.mode === "unrestricted" || draftRegion(draft) !== null;
}

export function regionDirty(draft: RegionDraft, current: RegionCurrent): boolean {
  if (draft.mode !== current.mode) return true;
  return draft.mode === "region" && draftRegion(draft) !== current.region;
}

export function regionPayload(draft: RegionDraft): RegionPayload {
  return { mode: draft.mode, region: draft.mode === "region" ? draftRegion(draft) : null };
}
