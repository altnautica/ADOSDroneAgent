// The Transmit screen's TX power write. The agent bounds the power to
// [1, `video.wfb.tx_power_max_dbm`] (the region clamp) and refuses it while
// armed; this module mirrors both so the slide stays disabled with the reason
// shown, rather than letting the operator slide into a refusal.

import type { WfbStatus } from "@/shared/api/types";

import type { RadioWriteBlock } from "./radio-gate";

/** The agent's floor for a TX power write (dBm). */
export const TX_POWER_MIN_DBM = 1;

export interface TxPowerBounds {
  min: number;
  max: number;
}

/** The writable range from the radio's report, or null when the radio has
 *  not reported a usable ceiling (no transmitter running). */
export function txPowerBounds(w: WfbStatus | null | undefined): TxPowerBounds | null {
  const max = w?.tx_power_max_dbm;
  if (typeof max !== "number" || !Number.isFinite(max) || max < TX_POWER_MIN_DBM) return null;
  return { min: TX_POWER_MIN_DBM, max: Math.floor(max) };
}

/** Whether the radio reports no transmitter (adapter missing or disabled). */
export function txAdapterMissing(w: WfbStatus | null | undefined): boolean {
  const state = (w?.state ?? "").toLowerCase();
  return state === "disabled" || (state === "error" && !w?.interface);
}

export type TxPowerBlock = Exclude<RadioWriteBlock, null> | "noRadio" | "outOfRange" | "unchanged" | null;

/** Why the TX power slide is unavailable, or null when `value` may be sent. */
export function txPowerBlock(
  value: number,
  current: number | null,
  bounds: TxPowerBounds | null,
  gate: RadioWriteBlock,
): TxPowerBlock {
  if (!bounds) return "noRadio";
  if (gate) return gate;
  if (!Number.isInteger(value) || value < bounds.min || value > bounds.max) return "outOfRange";
  if (current != null && value === current) return "unchanged";
  return null;
}
