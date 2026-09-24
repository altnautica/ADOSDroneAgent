import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/lib/api";

export type BatteryRuleId =
  | "cell_critical"
  | "cell_low"
  | "cell_divergence"
  | "voltage_drop"
  | "temp_spike"
  | "predictive_low";

export type BatterySeverity = "warning" | "critical";

export type BatteryPredictionState = "idle" | "normal" | "high" | "past";

export interface BatteryThresholds {
  enabled: boolean;
  low_cell_mv: number;
  critical_cell_mv: number;
  cell_divergence_mv: number;
  voltage_drop_mv_per_s: number;
  temp_spike_dc_per_s: number;
  predictive_window_s: number;
  reserve_percent: number;
}

export interface BatteryAnomaly {
  rule: BatteryRuleId;
  severity: BatterySeverity;
  value: number;
  threshold: number;
  first_seen_ms: number;
  last_seen_ms: number;
  /** Set while the rule has stopped firing but is still inside the
   * clear-hysteresis window; null while it is actively firing. */
  cleared_at_ms: number | null;
}

export interface BatteryHistoryEvent {
  rule: BatteryRuleId;
  severity: BatterySeverity;
  state: "raised" | "cleared";
  at_ms: number;
  value: number;
}

export interface BatteryPack {
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
  history: BatteryHistoryEvent[];
}

/** `GET /api/v1/battery`: the agent battery engine's per-pack health. Always
 * answers 200; `enabled: false` carries an empty pack list and `stale` flags
 * that no fresh vehicle state arrived for 5 s. `updated_at_ms` is the epoch
 * of the last ingested sample, 0 before the first one. */
export interface BatteryHealth {
  enabled: boolean;
  stale: boolean;
  updated_at_ms: number;
  thresholds: BatteryThresholds;
  /** Newest-first history per pack; anomalies ordered by rule. */
  packs: BatteryPack[];
}

export function useBatteryHealth(enabled = true) {
  return useQuery<BatteryHealth>({
    queryKey: ["battery-health"],
    queryFn: ({ signal }) =>
      apiFetch<BatteryHealth>("/api/v1/battery", { signal }),
    refetchInterval: 2_000,
    retry: false,
    enabled,
  });
}
