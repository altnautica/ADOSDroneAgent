// Switching Mode 1 ↔ Mode 2 swaps throttle and pitch under the pilot's thumbs,
// so it is refused while the vehicle is armed or airborne (or its arm state is
// unknown) and otherwise takes a second tap on the same choice to confirm.

export type StickModeTap =
  | { kind: "refused" }
  | { kind: "confirm"; target: string }
  | { kind: "switch"; target: string }
  | { kind: "none" };

/** What a tap on `target` does, given the current mode, the choice already
 *  waiting for its second tap, and whether the vehicle is known to be on the
 *  ground and disarmed. */
export function stickModeTap(current: string, target: string, pending: string | null, grounded: boolean): StickModeTap {
  if (target === current) return { kind: "none" };
  if (!grounded) return { kind: "refused" };
  return pending === target ? { kind: "switch", target } : { kind: "confirm", target };
}
