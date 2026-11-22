/**
 * @module hud/movers
 * @description The moving obstacles the pilot tracks (`movers`) as every map
 * draws them and every status line counts them, the same on every surface:
 * a mark per mover in its kind's colour and shape (person a disc, vehicle a
 * square, anything else a triangle), an arrow to where it will be in
 * {@link MOVER_LEAD_S} on its velocity, a ring at its one-sigma uncertainty,
 * faded once it was last measured over {@link MOVER_FADE_MS} ago; and the
 * line "Movers: 3 · nearest person 7.4 m".
 *
 * @license GPL-3.0-only
 */

import tokens from "../cockpit-spec/tokens.json";
import type { Mover, MoverKind } from "./pilot-fields";
import { specText } from "./spec-text";

/** The velocity arrow shows this much travel, s. */
export const MOVER_LEAD_S = 2;
/** A mover last measured longer ago than this, ms, draws faded. */
export const MOVER_FADE_MS = 1000;
/** Below this speed, m/s, a mover draws no arrow. */
const STILL_MPS = 0.1;

/** A person in the avoiding hue (the pilot keeps its distance), a vehicle in
 *  the blocked hue, anything else in the stale grey. */
export const MOVER_KIND_COLOR: Readonly<Record<MoverKind, string>> = {
  person: tokens.state.avoiding,
  vehicle: tokens.state.blocked,
  unknown: tokens.state.stale,
};

/** One mover as a map draws it, in map NED metres. */
export interface MoverMark {
  id: number;
  kind: MoverKind;
  ne: [number, number];
  /** Where it will be in {@link MOVER_LEAD_S}; null while it is still. */
  leadNe: [number, number] | null;
  sigmaM: number;
  faded: boolean;
}

export function moverMarks(movers: readonly Mover[]): MoverMark[] {
  return movers.map((m) => ({
    id: m.id,
    kind: m.kind,
    ne: [m.ne[0], m.ne[1]],
    leadNe:
      Math.hypot(m.velNe[0], m.velNe[1]) < STILL_MPS
        ? null
        : [m.ne[0] + m.velNe[0] * MOVER_LEAD_S, m.ne[1] + m.velNe[1] * MOVER_LEAD_S],
    sigmaM: Math.max(0, m.sigmaM),
    faded: m.ageMs > MOVER_FADE_MS,
  }));
}

/** `Person`, `Vehicle` or `Moving object`. */
export function moverKindLabel(kind: MoverKind): string {
  return specText(`pilot.movers.kind.${kind}`);
}

/** The horizontal distance, m, from the drone to the nearest person, or null
 *  with no person tracked or no drone position. */
export function nearestPersonM(movers: readonly Mover[], drone: { north: number; east: number } | null): number | null {
  if (!drone) return null;
  let best: number | null = null;
  for (const m of movers) {
    if (m.kind !== "person") continue;
    const d = Math.hypot(m.ne[0] - drone.north, m.ne[1] - drone.east);
    if (best === null || d < best) best = d;
  }
  return best;
}

/** `Movers: 3 · nearest person 7.4 m` (`Movers: 3` without a person or a
 *  drone position), or null while the pilot tracks none. */
export function moversLine(movers: readonly Mover[], drone: { north: number; east: number } | null): string | null {
  if (movers.length === 0) return null;
  const n = String(movers.length);
  const d = nearestPersonM(movers, drone);
  return d === null ? specText("pilot.movers.count", { n }) : specText("pilot.movers.count_person", { n, d: d.toFixed(1) });
}
