// The Video panel's hero hold: while the panel is mounted with a stream to
// show and the page is visible, the drone stays on its `hero` video profile
// (a WFB drone otherwise boots on a 1 fps thumbnail), under this page's lease
// owner. The shared hold renews, retries and releases it
// (`@/shared/hero-hold`).

import { LEASE_OWNER } from "@/lib/lease-owner";
import { createHeroHold } from "@/shared/hero-hold";
import type { AgentProfile } from "@/shared/use-profile";

export const PANEL_HERO_HOLD = createHeroHold(LEASE_OWNER);

/** A ground station has no encoder of its own to hold (its route answers
 *  404); an unknown profile waits until it is known. */
export function panelWantsHero(canStream: boolean, profile: AgentProfile | null): boolean {
  return canStream && profile !== null && profile !== "ground_station";
}
