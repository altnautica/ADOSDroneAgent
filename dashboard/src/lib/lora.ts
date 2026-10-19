// LoRa lane: wire types for `/api/v1/lora*` and the pure helpers the `/lora`
// page renders from. Kept free of React so the governor logic is unit-tested.

import { apiFetch } from "@/shared/api-fetch";

export type LoraBlockedReason =
  | "region_unset"
  | "module_band_mismatch"
  | "duty_exhausted"
  | "dwell_time"
  | "lbt_busy"
  | "power_over_limit";

export interface LoraRegion {
  code: string;
  name: string;
  plan_id: string;
  source: string;
  band_mhz: [number, number];
  citation?: string;
  meshtastic_preset?: string | null;
}

export type LoraRule =
  | {
      kind: "duty_cycle";
      window_s: number;
      limit_pct: number;
      used_pct: number;
      airtime_ms_window: number;
      budget_ms_window: number;
      exhausted: boolean;
      sub_band: string;
    }
  | { kind: "dwell_time"; limit_ms: number; per_s: number; last_tx_ms: number; ok: boolean }
  | { kind: "lbt"; threshold_dbm: number; listen_us: number; last_rssi_dbm: number; busy_count: number }
  | { kind: "max_power"; limit_mw: number; configured_mw: number; ok: boolean };

export interface LoraStatus {
  present: boolean;
  transport: string | null;
  module_band: string | null;
  region: LoraRegion | null;
  channel: { freq_mhz: number; bw_khz: number; sf: number; erp_mw: number; max_erp_mw: number } | null;
  node_id: number | null;
  tx_allowed: boolean;
  blocked_reason: LoraBlockedReason | null;
  governor: {
    rules: LoraRule[];
    next_tx_allowed_ms: number;
    last_refusal: { rule: string; at_ms: number } | null;
  } | null;
  tx: { frames: number; refused: number; refused_by_rule: Record<string, number> } | null;
  rx: { frames: number; crc_errors: number } | null;
  meshtastic: { enabled: boolean; connected: boolean; port: string | null; region_preset: string | null } | null;
}

export interface LoraRegionRow {
  code: string;
  name: string;
  plan_id: string;
  band_mhz: [number, number];
  rules: string[];
  supported_by_module: boolean;
  meshtastic_preset: string | null;
  /** The ExpressLRS domain this region expects per module band; null = no claim. */
  elrs_domains: { "2g4": string | null; "900": string | null };
}

/** One RC module from `GET /api/v1/ground-station/crsf` `modules[]`. */
export interface CrsfModule {
  id: string;
  domain: string | null;
}

export const CRSF_STATUS_PATH = "/api/v1/ground-station/crsf";

export interface LoraBeacon {
  node_id: number;
  kind: string;
  battery_pct: number | null;
  mode: string | null;
  rssi_dbm: number;
  snr_db: number;
  received_at_ms: number;
}

export const LORA_PATH = "/api/v1/lora";
export const LORA_REGIONS_PATH = "/api/v1/lora/regions";
export const LORA_BEACONS_PATH = "/api/v1/lora/beacons";

/** Set the LoRa region (`null` follows the country setting). Admin-scope; the
 *  agent refuses it while armed. Returns the new status. */
export function putLoraRegion(region: string | null): Promise<LoraStatus> {
  return apiFetch<LoraStatus>("/api/v1/lora", { method: "PUT", body: { region } });
}

/** The TX line: allowed, a self-clearing wait (duty/dwell) with its
 *  countdown, a busy channel, or a block that needs operator action. */
export type LoraTxState =
  | { kind: "allowed"; waitMs: number }
  | { kind: "paused"; reason: LoraBlockedReason; waitMs: number }
  | { kind: "busy" }
  | { kind: "blocked"; reason: LoraBlockedReason };

export function loraTxState(s: LoraStatus): LoraTxState {
  const waitMs = Math.max(0, s.governor?.next_tx_allowed_ms ?? 0);
  if (s.tx_allowed && !s.blocked_reason) return { kind: "allowed", waitMs };
  const reason = s.blocked_reason ?? "region_unset";
  if (reason === "duty_exhausted" || reason === "dwell_time") {
    return { kind: "paused", reason, waitMs };
  }
  if (reason === "lbt_busy") return { kind: "busy" };
  return { kind: "blocked", reason };
}

/** `mm:ss` for a countdown in milliseconds (rounded up to the next second). */
export function formatCountdown(ms: number): string {
  const total = Math.max(0, Math.ceil(ms / 1000));
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
}

/** Fill fraction 0..1 of a duty-cycle meter. */
export function dutyFraction(rule: Extract<LoraRule, { kind: "duty_cycle" }>): number {
  if (rule.budget_ms_window <= 0) return rule.exhausted ? 1 : 0;
  return Math.min(1, Math.max(0, rule.airtime_ms_window / rule.budget_ms_window));
}

export type MeterTone = "neutral" | "warn" | "error";

/** Below 70 % of the limit neutral, 70–99 % amber, at the limit red. */
export function meterTone(fraction: number): MeterTone {
  if (fraction >= 1) return "error";
  if (fraction >= 0.7) return "warn";
  return "neutral";
}

/** Region codes whose plan carries rows not cleared for airborne use. */
const AIRBORNE_RESTRICTED: Record<string, true> = { RU: true, GB: true, NO: true };

/** ExpressLRS domain → band; a domain outside the table is unknown (no claim). */
const ELRS_BAND: Record<string, "2g4" | "900"> = {
  ISM2G4: "2g4",
  CE_LBT: "2g4",
  FCC915: "900",
  AU915: "900",
  EU868: "900",
  IN866: "900",
  AU433: "900",
  EU433: "900",
};

export type LoraChip =
  | { kind: "module_band"; module: string; plan: string }
  | { kind: "meshtastic"; node: string; expected: string }
  | { kind: "country"; lora: string; country: string }
  | { kind: "airborne"; code: string }
  | { kind: "elrs"; module: string; domain: string; code: string; expected: string };

/** Advisory mismatch chips. `country` is the agent's country setting
 *  (`network.regulatory.region`), null when unset. `row` is the active
 *  region's row from the regions table and `modules` the RC modules reported
 *  by the CRSF lane (empty where the node runs none). */
export function loraChips(
  s: LoraStatus,
  country: string | null,
  row: LoraRegionRow | null = null,
  modules: readonly CrsfModule[] = [],
): LoraChip[] {
  const chips: LoraChip[] = [];
  if (s.blocked_reason === "module_band_mismatch" && s.module_band && s.region) {
    chips.push({ kind: "module_band", module: s.module_band, plan: s.region.plan_id });
  }
  if (s.region && Object.hasOwn(AIRBORNE_RESTRICTED, s.region.code.toUpperCase())) {
    chips.push({ kind: "airborne", code: s.region.code });
  }
  const mesh = s.meshtastic;
  const expected = s.region?.meshtastic_preset ?? null;
  if (mesh?.enabled && mesh.region_preset && expected && mesh.region_preset !== expected) {
    chips.push({ kind: "meshtastic", node: mesh.region_preset, expected });
  }
  if (s.region && s.region.source === "lora.region" && country && country !== s.region.code) {
    chips.push({ kind: "country", lora: s.region.code, country });
  }
  if (s.region && row) {
    for (const m of modules) {
      if (!m.domain) continue;
      const band = Object.hasOwn(ELRS_BAND, m.domain) ? ELRS_BAND[m.domain] : null;
      const want = band ? (row.elrs_domains?.[band] ?? null) : null;
      if (want && want !== m.domain) {
        chips.push({ kind: "elrs", module: m.id, domain: m.domain, code: s.region.code, expected: want });
      }
    }
  }
  return chips;
}

/** Seconds since a beacon was heard, never negative. */
export function beaconAgeS(b: LoraBeacon, nowMs: number): number {
  return Math.max(0, Math.round((nowMs - b.received_at_ms) / 1000));
}
