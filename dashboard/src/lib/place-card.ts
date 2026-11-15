// The Home place card of the confined-place profile. The dashboard has no
// Launch sheet (it hands launching to the cockpit) and no pilot settings page,
// so the place-class picker sits on the Home vehicle status, beside the
// countdown card's other pilot actions: Open / Confined space with the explain
// line (the obstacle margin the pilot flies), the "Confined" badge, and the
// pilot's route check of the loaded mission. A press sends
// `place_class:<class>` over the page's pilot link; the pilot refuses it while
// armed, and the refusal reads through the refusal table.

import type { ActionResult } from "@/lib/pilot-action";
import { PLACE_CLASSES, type PilotState, type PlaceClass } from "@/shared/hud/cockpit-frames";
import { refusalText } from "@/shared/hud/refusal";
import { confinedChipText, placeClassAction, placeClassLabel, routeCheckReport, type RouteCheckReport } from "@/shared/hud/route-check";
import { specText } from "@/shared/hud/spec-text";
import { placeExplainLine, type PlaceMargins } from "@/shared/place-class";

export interface PlaceOption {
  cls: PlaceClass;
  label: string;
  /** The pilot action the option sends. */
  action: string;
  selected: boolean;
}

export interface PlaceCardModel {
  title: string;
  options: PlaceOption[];
  explain: string;
  /** "Confined" while the pilot flies the confined profile, else null. */
  badge: string | null;
  routeCheck: RouteCheckReport | null;
}

/** The card for a fresh pilot frame, or null with none. */
export function placeCard(pilot: PilotState | null, margins: PlaceMargins): PlaceCardModel | null {
  if (!pilot) return null;
  return {
    title: specText("flow.launch.place_class"),
    options: PLACE_CLASSES.map((cls) => ({ cls, label: placeClassLabel(cls), action: placeClassAction(cls), selected: cls === pilot.placeClass })),
    explain: placeExplainLine(pilot.placeClass, pilot, margins),
    badge: confinedChipText(pilot),
    routeCheck: routeCheckReport(pilot.routeCheck),
  };
}

/** Send the class the operator picked; a class already in force sends
 *  nothing. Resolves to the refusal in words, or null. */
export async function choosePlace(option: PlaceOption, send: (action: string) => Promise<ActionResult>): Promise<string | null> {
  if (option.selected) return null;
  const r = await send(option.action);
  return r.ok ? null : `${option.label}: ${refusalText(r.err)}`;
}
