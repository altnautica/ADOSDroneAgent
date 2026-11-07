// Labels the controller editor shows: an action's short label and the label
// on each hotspot's leader line.

import { SPEC_ACTION_BY_ID, type ControlId, type PadBinding } from "@/lib/pad/ds4-map";

/** A short label for an action id (spec action, registry action or none). */
export function actionLabel(action: string, registry: ReadonlyMap<string, string>): string {
  if (action === "none") return "None";
  return SPEC_ACTION_BY_ID[action]?.label ?? registry.get(action) ?? action;
}

/** The label a hotspot's leader line shows: its tap binding, else its first. */
export function hotspotLabel(rows: readonly PadBinding[], control: ControlId, registry: ReadonlyMap<string, string>): string | null {
  const own = rows.filter((r) => r.control === control && r.with.length === 0 && r.action !== "none");
  const row = own.find((r) => r.gesture === "tap" || r.gesture === "analog") ?? own[0];
  return row ? actionLabel(row.action, registry) : null;
}
