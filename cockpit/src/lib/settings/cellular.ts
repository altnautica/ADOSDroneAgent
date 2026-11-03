// The cellular section's logic. A ground station writes its live modem through
// the ground-station modem routes (each answer is the persisted read-back);
// every other profile writes the `network.cellular.*` config leaves. Turning the
// uplink off and changing the APN can drop a link the operator may be riding,
// so `submitCellular` stages those for a slide and writes the rest at once.

import type { ModemPresence, ModemView } from "@/shared/api/modem";

export const CELLULAR_POLL_MS = 10_000;

/** The configured data cap (MB) as a GB input string; blank when unset. */
export function capMbToGb(capMb: number | null | undefined): string {
  if (typeof capMb !== "number" || !Number.isFinite(capMb) || capMb <= 0) return "";
  return String(Math.round((capMb / 1024) * 100) / 100);
}

/** A GB data cap (0 clears it), or null when not a non-negative number. */
export function parseCapGb(raw: string): number | null {
  const t = raw.trim();
  if (t.length === 0) return null;
  const n = Number(t);
  if (!Number.isFinite(n) || n < 0) return null;
  return n;
}

/** The presence line's locale key. Unknown stays unknown: `not_probed` and any
 *  other reason mean nothing was looked at. */
export function presenceKey(
  presence: ModemPresence | null,
):
  | "settings.shared.unknown"
  | "settings.cellular.presenceDetected"
  | "settings.cellular.presenceNoModem"
  | "settings.cellular.presenceNoModemManager" {
  if (!presence) return "settings.shared.unknown";
  if (presence.present === true) return "settings.cellular.presenceDetected";
  switch (presence.reason) {
    case "no_modem":
      return "settings.cellular.presenceNoModem";
    case "modemmanager_not_installed":
      return "settings.cellular.presenceNoModemManager";
    default:
      return "settings.shared.unknown";
  }
}

export interface ModemFacts {
  state: string | null;
  operator: string | null;
  technology: string | null;
  signalQuality: number | null;
  ip: string | null;
  capMb: number | null;
  usedMb: number | null;
  percent: number | null;
}

/** The reported facts, with the no-modem sentinels (signal -1, technology
 *  "unknown", operator "") read as unknown, never as facts. */
export function modemFacts(modem: ModemView): ModemFacts {
  const text = (v: string | null | undefined) => (typeof v === "string" && v.length > 0 ? v : null);
  const num = (v: number | null | undefined) => (typeof v === "number" && Number.isFinite(v) ? v : null);
  const technology = text(modem.technology);
  const signal = num(modem.signal_quality);
  return {
    state: text(modem.state),
    operator: text(modem.operator),
    technology: technology === "unknown" ? null : technology,
    signalQuality: signal !== null && signal >= 0 ? signal : null,
    ip: text(modem.ip),
    capMb: num(modem.cap_mb),
    usedMb: num(modem.data_used_mb),
    percent: num(modem.percent),
  };
}

export type CellularChange = { enabled: boolean } | { apn: string } | { cap_gb: number };

/** Disable and an APN change can cut the uplink: slide. Enable and the data
 *  cap cannot: written at once. */
export function cellularGuard(change: CellularChange): "slide" | "direct" {
  if ("enabled" in change) return change.enabled ? "direct" : "slide";
  if ("apn" in change) return "slide";
  return "direct";
}

/** Route a change: a slide-guarded one is staged (nothing is written until the
 *  slide commits it), any other is written now. */
export function submitCellular(
  change: CellularChange,
  sinks: { write: (change: CellularChange) => void; stage: (change: CellularChange) => void },
): void {
  if (cellularGuard(change) === "slide") sinks.stage(change);
  else sinks.write(change);
}

/** The config leaf a non-ground-station write lands on. The data cap is a
 *  live-modem setting with no config leaf. */
export function configLeaf(change: CellularChange): { key: string; value: string | boolean } | null {
  if ("enabled" in change) return { key: "network.cellular.enabled", value: change.enabled };
  if ("apn" in change) return { key: "network.cellular.apn", value: change.apn };
  return null;
}

/** The success toast's locale key for a change on either branch. */
export function changeSavedKey(
  change: CellularChange,
  groundStation: boolean,
):
  | "settings.cellular.modemEnabled"
  | "settings.cellular.modemDisabled"
  | "settings.cellular.cellularEnabled"
  | "settings.cellular.cellularDisabled"
  | "settings.cellular.apnSaved"
  | "settings.cellular.capSaved"
  | "settings.cellular.capCleared" {
  if ("enabled" in change) {
    if (groundStation) return change.enabled ? "settings.cellular.modemEnabled" : "settings.cellular.modemDisabled";
    return change.enabled ? "settings.cellular.cellularEnabled" : "settings.cellular.cellularDisabled";
  }
  if ("apn" in change) return "settings.cellular.apnSaved";
  return change.cap_gb > 0 ? "settings.cellular.capSaved" : "settings.cellular.capCleared";
}

/** The failure toast's locale key for a change on either branch. */
export function changeFailedKey(
  change: CellularChange,
  groundStation: boolean,
):
  | "settings.cellular.modemUpdateFailed"
  | "settings.cellular.cellularUpdateFailed"
  | "settings.cellular.apnFailed"
  | "settings.cellular.capFailed" {
  if ("enabled" in change) {
    return groundStation ? "settings.cellular.modemUpdateFailed" : "settings.cellular.cellularUpdateFailed";
  }
  if ("apn" in change) return "settings.cellular.apnFailed";
  return "settings.cellular.capFailed";
}
