// Shared helpers for the operating-region (RF regulatory) surfaces:
// the Region settings page and the Home chip. The agent defaults to an
// unrestricted RF posture; an operator opts into a single region to
// re-enable the strict gate.

import type { RegulatoryInfo, RegulatoryMode } from "./types";

export interface CountryOption {
  code: string;
  label: string;
}

// A short list of common operating regions, plus an "Other ISO code"
// free-text escape hatch handled by the surfaces. Codes are ISO 3166-1
// alpha-2, uppercase. Globally framed: the operator is responsible for
// local RF compliance wherever they fly.
export const COMMON_REGIONS: ReadonlyArray<CountryOption> = [
  { code: "US", label: "United States" },
  { code: "IN", label: "India" },
  { code: "DE", label: "Germany" },
  { code: "GB", label: "United Kingdom" },
  { code: "AU", label: "Australia" },
  { code: "JP", label: "Japan" },
  { code: "CA", label: "Canada" },
];

// Normalize free-text into an ISO alpha-2 candidate (uppercase, A-Z).
// Returns null when the input is not a valid 2-letter code.
export function normalizeRegion(raw: string): string | null {
  const code = raw.trim().toUpperCase();
  return /^[A-Z]{2}$/.test(code) ? code : null;
}

// Resolve the effective mode from a status payload. Absent block or
// absent mode -> unrestricted (a fresh box).
export function modeFromStatus(reg: RegulatoryInfo | undefined): RegulatoryMode {
  return reg?.mode === "region" ? "region" : "unrestricted";
}

// Resolve the pinned region (uppercase ISO) from a status payload, or
// null when unrestricted / unset.
export function regionFromStatus(
  reg: RegulatoryInfo | undefined,
): string | null {
  if (modeFromStatus(reg) !== "region") return null;
  const code = (reg?.region ?? "").trim().toUpperCase();
  return /^[A-Z]{2}$/.test(code) ? code : null;
}

// Human label for a region code, falling back to the raw code.
export function regionLabel(code: string): string {
  const match = COMMON_REGIONS.find((r) => r.code === code);
  return match ? `${match.label} (${match.code})` : code;
}
