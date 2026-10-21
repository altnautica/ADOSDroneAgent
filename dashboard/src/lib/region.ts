// Shared helpers for the operating-region (RF regulatory) surfaces:
// the Region settings page and the Home chip. The agent defaults to an
// unrestricted RF posture; an operator opts into a single region to
// re-enable the strict gate.

import i18n from "@/i18n";

import type { RegulatoryInfo, RegulatoryMode } from "./types";


// A short list of common operating regions, plus an "Other ISO code"
// free-text escape hatch handled by the surfaces. Codes are ISO 3166-1
// alpha-2, uppercase. Globally framed: the operator is responsible for
// local RF compliance wherever they fly.
export const COMMON_REGIONS = [
  { code: "US", labelKey: "settings.region.countryUS" },
  { code: "IN", labelKey: "settings.region.countryIN" },
  { code: "DE", labelKey: "settings.region.countryDE" },
  { code: "GB", labelKey: "settings.region.countryGB" },
  { code: "AU", labelKey: "settings.region.countryAU" },
  { code: "JP", labelKey: "settings.region.countryJP" },
  { code: "CA", labelKey: "settings.region.countryCA" },
] as const;

export type CountryOption = (typeof COMMON_REGIONS)[number];

// Normalize free-text into an ISO alpha-2 candidate (uppercase, A-Z).
// Returns null when the input is not a valid 2-letter code.
export function normalizeRegion(raw: string): string | null {
  const code = raw.trim().toUpperCase();
  return /^[A-Z]{2}$/.test(code) ? code : null;
}

// Resolve the effective mode from the config's `network.regulatory` block.
// Absent block or absent mode -> unrestricted (a fresh box).
export function modeFromConfig(reg: RegulatoryInfo | undefined): RegulatoryMode {
  return reg?.mode === "region" ? "region" : "unrestricted";
}

// Resolve the pinned region (uppercase ISO) from the config's
// `network.regulatory` block, or null when unrestricted / unset.
export function regionFromConfig(
  reg: RegulatoryInfo | undefined,
): string | null {
  if (modeFromConfig(reg) !== "region") return null;
  const code = (reg?.region ?? "").trim().toUpperCase();
  return /^[A-Z]{2}$/.test(code) ? code : null;
}

// Human label for a region code, falling back to the raw code.
export function regionLabel(code: string): string {
  const match = COMMON_REGIONS.find((r) => r.code === code);
  return match
    ? i18n.t("settings.region.countryWithCode", {
        label: i18n.t(match.labelKey),
        code: match.code,
      })
    : code;
}
