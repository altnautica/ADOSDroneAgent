// The Battery settings page model: the battery engine's full per-pack read
// (`GET /api/v1/battery`, polled every 2 s), the formatting of each rule's
// value in the unit the engine reports it in, and the threshold fields with
// the bounds the agent config model enforces.

import type { Tone } from "@/lib/tone";
import { apiFetch } from "@/shared/api-fetch";
import { DASH } from "@/shared/format";

export const BATTERY_POLL_MS = 2000;

export type BatteryRuleId =
  | "cell_critical"
  | "cell_low"
  | "cell_divergence"
  | "voltage_drop"
  | "temp_spike"
  | "predictive_low";

export type BatterySeverity = "warning" | "critical";

export type BatteryPredictionState = "idle" | "normal" | "high" | "past";

export interface BatteryAnomaly {
  rule: BatteryRuleId;
  severity: BatterySeverity;
  value: number;
  threshold: number;
  first_seen_ms: number;
  last_seen_ms: number;
  /** Set while the rule has stopped firing but is still inside the
   *  clear-hysteresis window; null while it is actively firing. */
  cleared_at_ms: number | null;
}

export interface BatteryHistoryEvent {
  rule: BatteryRuleId;
  severity: BatterySeverity;
  state: "raised" | "cleared";
  at_ms: number;
  value: number;
}

export interface BatteryPackDetail {
  id: number;
  cells_plausible: boolean;
  cell_voltages_v: number[];
  weakest_cell_index: number | null;
  min_cell_v: number | null;
  max_cell_v: number | null;
  divergence_mv: number | null;
  voltage_v: number | null;
  current_a: number | null;
  remaining_pct: number | null;
  temperature_c: number | null;
  consumed_mah: number | null;
  consumed_wh: number | null;
  prediction: {
    state: BatteryPredictionState;
    eta_s: number | null;
    drop_pct_per_s: number | null;
    mean_current_a: number | null;
  };
  anomalies: BatteryAnomaly[];
  /** Newest first. */
  history: BatteryHistoryEvent[];
}

/** The engine always answers 200; `enabled: false` carries no packs and
 *  `stale` flags that no fresh vehicle state arrived for 5 s. */
export interface BatteryHealthDetail {
  enabled: boolean;
  stale: boolean;
  updated_at_ms: number;
  packs: BatteryPackDetail[];
}

export function getBatteryDetail(signal?: AbortSignal): Promise<BatteryHealthDetail> {
  return apiFetch<BatteryHealthDetail>("/api/v1/battery", { signal });
}

export type BatteryThresholdKey =
  | "low_cell_mv"
  | "critical_cell_mv"
  | "cell_divergence_mv"
  | "voltage_drop_mv_per_s"
  | "temp_spike_dc_per_s"
  | "predictive_window_s"
  | "reserve_percent";

export interface ThresholdField {
  key: BatteryThresholdKey;
  labelKey: string;
  hintKey: string;
  min: number;
  max: number;
  step: number;
}

/** Threshold fields with the bounds the agent config model enforces. */
export const THRESHOLD_FIELDS = [
  { key: "low_cell_mv", labelKey: "settingsMore.battery.lowCellLabel", hintKey: "settingsMore.battery.lowCellHint", min: 2500, max: 4200, step: 10 },
  { key: "critical_cell_mv", labelKey: "settingsMore.battery.criticalCellLabel", hintKey: "settingsMore.battery.criticalCellHint", min: 2500, max: 4000, step: 10 },
  { key: "cell_divergence_mv", labelKey: "settingsMore.battery.divergenceLabel", hintKey: "settingsMore.battery.divergenceHint", min: 10, max: 500, step: 5 },
  { key: "voltage_drop_mv_per_s", labelKey: "settingsMore.battery.sagLabel", hintKey: "settingsMore.battery.sagHint", min: 100, max: 5000, step: 50 },
  { key: "temp_spike_dc_per_s", labelKey: "settingsMore.battery.tempSpikeLabel", hintKey: "settingsMore.battery.tempSpikeHint", min: 5, max: 200, step: 1 },
  { key: "predictive_window_s", labelKey: "settingsMore.battery.windowLabel", hintKey: "settingsMore.battery.windowHint", min: 5, max: 300, step: 5 },
  { key: "reserve_percent", labelKey: "settingsMore.battery.reserveLabel", hintKey: "settingsMore.battery.reserveHint", min: 5, max: 50, step: 1 },
] as const satisfies readonly ThresholdField[];

/** Whether `n` is a value the config model accepts for this field. */
export function thresholdValid(field: ThresholdField, n: number): boolean {
  return Number.isInteger(n) && n >= field.min && n <= field.max;
}

export const RULE_LABEL = {
  cell_critical: "settingsMore.battery.ruleCellCritical",
  cell_low: "settingsMore.battery.ruleCellLow",
  cell_divergence: "settingsMore.battery.ruleCellDivergence",
  voltage_drop: "settingsMore.battery.ruleVoltageSag",
  temp_spike: "settingsMore.battery.ruleTempSpike",
  predictive_low: "settingsMore.battery.ruleReserveSoon",
} as const satisfies Record<BatteryRuleId, string>;

export const SEVERITY_LABEL = {
  warning: "settingsMore.battery.severityWarning",
  critical: "settingsMore.battery.severityCritical",
} as const satisfies Record<BatterySeverity, string>;

export const EVENT_STATE_LABEL = {
  raised: "settingsMore.battery.eventRaised",
  cleared: "settingsMore.battery.eventCleared",
} as const satisfies Record<BatteryHistoryEvent["state"], string>;

export const PREDICTION = {
  idle: { labelKey: "settingsMore.battery.predIdle", tone: "muted" },
  normal: { labelKey: "settingsMore.battery.predDraining", tone: "ok" },
  high: { labelKey: "settingsMore.battery.predDrainingFast", tone: "warn" },
  past: { labelKey: "settingsMore.battery.predPastReserve", tone: "err" },
} as const satisfies Record<BatteryPredictionState, { labelKey: string; tone: Tone }>;

/** An anomaly's value or threshold in the unit the engine reports for that
 *  rule: cell rules in V, divergence in mV, sag in V/s, temperature in °C/s,
 *  prediction in seconds to reserve. */
export function fmtRuleValue(rule: BatteryRuleId, v: number): string {
  switch (rule) {
    case "cell_critical":
    case "cell_low":
      return `${v.toFixed(2)} V`;
    case "cell_divergence":
      return `${Math.round(v)} mV`;
    case "voltage_drop":
      return `${v.toFixed(2)} V/s`;
    case "temp_spike":
      return `${v.toFixed(1)} °C/s`;
    case "predictive_low":
      return `${Math.round(v)} s`;
  }
}

/** Time to reserve as `Mm SSs`, or a dash. */
export function fmtEta(s: number | null): string {
  if (s == null) return DASH;
  const m = Math.floor(s / 60);
  const rem = s % 60;
  return m > 0 ? `${m}m ${String(rem).padStart(2, "0")}s` : `${rem}s`;
}

export type LiveView = "loading" | "error" | "off" | "empty" | "packs";

/** Which state the live-pack block shows for the current poll. A failed poll
 *  with an earlier snapshot keeps showing the packs (flagged stale). */
export function liveView(data: BatteryHealthDetail | null, ready: boolean): LiveView {
  if (data == null) return ready ? "error" : "loading";
  if (!data.enabled) return "off";
  return data.packs.length === 0 ? "empty" : "packs";
}

/** The newest events shown under a pack. */
export const RECENT_EVENTS = 8;
