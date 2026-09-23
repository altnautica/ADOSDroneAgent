import type { HardwareItem, HardwareItemState } from "@/lib/types";

const STATE_RANK: Record<HardwareItemState, number> = {
  ok: 0,
  unknown: 1,
  checking: 2,
  warning: 3,
  missing: 4,
};

// Counts split by required/optional, plus the worst state among required items.
export function summarizeHardware(items: HardwareItem[]): {
  requiredOk: number;
  requiredTotal: number;
  optionalOk: number;
  optionalTotal: number;
  worstState: HardwareItemState;
} {
  let requiredOk = 0;
  let requiredTotal = 0;
  let optionalOk = 0;
  let optionalTotal = 0;
  let worstRank = 0;
  let worstState: HardwareItemState = "ok";
  for (const item of items) {
    if (item.required) {
      requiredTotal += 1;
      if (item.state === "ok") requiredOk += 1;
    } else {
      optionalTotal += 1;
      if (item.state === "ok") optionalOk += 1;
    }
    const rank = STATE_RANK[item.state] ?? 0;
    if (item.required && rank > worstRank) {
      worstRank = rank;
      worstState = item.state;
    }
  }
  return { requiredOk, requiredTotal, optionalOk, optionalTotal, worstState };
}
