// The pilot's part of the pre-flight checklist. A pilot state that went stale
// says nothing about now: every item it listed reads not reported and fails,
// and a stale state that listed none still adds one failing item, so the
// first arm never passes on a frozen frame. A fresh battery failsafe item
// whose action does nothing carries the one-tap fix while disarmed; a fresh
// airframe balance warning or block carries "I rebalanced it".

import type { ChecklistItem } from "@/lib/cockpit-flow";
import { batteryFailsafeFixable } from "@/shared/hud/battery-failsafe-fix";
import { airframeResettable } from "@/shared/hud/liftoff";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { pilotFresh } from "@/shared/hud/pilot-fresh";
import { specText } from "@/shared/hud/spec-text";
import type { Stamped } from "@/stores/nav-stream-store";

export function pilotReadinessItems(pilot: Stamped<PilotState> | null, nowMs: number): ChecklistItem[] {
  if (!pilot) return [];
  const fresh = pilotFresh(pilot.at, nowMs);
  const items: ChecklistItem[] = pilot.data.readiness.map((r) => {
    const label = r.msg || r.id;
    if (!fresh) return { id: `pilot:${r.id}`, label: specText("flow.preflight.not_reported", { item: label }), state: "fail" };
    const item: ChecklistItem = { id: `pilot:${r.id}`, label, state: r.ok ? "pass" : r.severity === "block" ? "fail" : "warn" };
    if (batteryFailsafeFixable(r, pilot.data.armed)) item.fix = "battery_failsafe";
    else if (airframeResettable(r, pilot.data.armed)) item.fix = "airframe_reset";
    return item;
  });
  if (!fresh && items.length === 0) {
    items.push({
      id: "pilot",
      label: specText("flow.preflight.not_reported", { item: specText("cockpit.status.source.pilot") }),
      state: "fail",
    });
  }
  return items;
}
