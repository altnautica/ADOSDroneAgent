// The first-connect controller tour's rules: which cards it walks through,
// what it shows for the current controller and the per-device `tourSeen`
// flag, when it waits (the vehicle is in the air, on any screen), and how the
// pad drives the card (D-pad moves between its buttons, Cross presses the
// focused one, Circle closes it).

export const TOUR_CARDS = ["sticks", "safety", "skills"] as const;

/** What the tour shows: nothing, the "press any button" prompt (the browser
 *  announced a controller it has not yet reported), or the tour card. It
 *  never opens while the vehicle is in the air, on any screen; it waits until
 *  landed. */
export function tourStage(padReported: boolean, padAnnounced: boolean, seen: boolean, airborne: boolean): "none" | "press" | "tour" {
  if (seen || airborne) return "none";
  if (padReported) return "tour";
  return padAnnounced ? "press" : "none";
}

/** Whether the vehicle is (possibly) in the air: from the live pilot state,
 *  unless it reports disarmed or landed (an unknown arm state counts as
 *  airborne), else (no live pilot) the heartbeat's arm state, where an
 *  unknown arm state (the agent withholds it while the flight controller link
 *  is down) also counts as airborne. */
export function tourAirborne(pilot: { armed: boolean | null; landed: boolean | null } | null, telemetryArmed: boolean | null | undefined): boolean {
  if (pilot) return pilot.armed !== false && pilot.landed !== true;
  return telemetryArmed !== false;
}

/** The heartbeat's arm state only while the poll is live and fresh, else null
 *  (unknown): a kept snapshot from an unanswered poll is not a reading. The
 *  same freshness rule as `knownOnGround`. */
export function liveArmed(s: { live: boolean; stale: boolean; telemetry: { armed?: boolean | null } | null }): boolean | null {
  return s.live && !s.stale ? (s.telemetry?.armed ?? null) : null;
}

/** The tips' arm inputs. From the live pilot: its arm state as reported,
 *  null while unknown (its FC link is down), and airborne null with either
 *  unknown, so no tip shows and none is marked seen. With no live pilot the
 *  heartbeat decides: a known arm dismisses a tip, and an unknown arm state
 *  counts as possibly airborne. */
export function tipsArmState(
  pilot: { armed: boolean | null; landed: boolean | null } | null,
  telemetryArmed: boolean | null | undefined,
): { armed: boolean | null; airborne: boolean | null } {
  if (pilot) {
    const unknown = pilot.armed === null || pilot.landed === null;
    return { armed: pilot.armed, airborne: unknown ? null : pilot.armed && !pilot.landed };
  }
  return { armed: telemetryArmed === true, airborne: telemetryArmed !== false };
}

/** Whether the vehicle is known to be on the ground and disarmed: from a
 *  fresh pilot state reporting disarmed and not flying or, with no fresh
 *  pilot, live telemetry reporting disarmed. An unknown arm state (cockpit
 *  loaded mid-flight before the first frame, link down) counts as airborne. */
export function knownOnGround(
  pilot: { armed: boolean | null; landed: boolean | null } | null,
  telemetry: { live: boolean; armed: boolean | null | undefined },
): boolean {
  if (pilot) return pilot.armed === false && pilot.landed !== false;
  return telemetry.live && telemetry.armed === false;
}

/** Whether the tour card may capture the pad (so Cross does not also start a
 *  skill): only while the vehicle is known on the ground and disarmed. Else
 *  the card shows without capturing, so kill, RTH, pause and arm stay live. */
export function tourCapturesPad(
  pilot: { armed: boolean | null; landed: boolean | null } | null,
  telemetry: { live: boolean; armed: boolean | null | undefined },
): boolean {
  return knownOnGround(pilot, telemetry);
}

/** The card's buttons, in focus order: Next (Done on the last card), Skip. */
export const TOUR_BUTTONS = ["advance", "skip"] as const;
export type TourButton = (typeof TOUR_BUTTONS)[number];

export type TourPadCommand = "prev" | "next" | "activate" | "close";

/** The pad command a W3C standard-layout button gives the tour card: the
 *  D-pad moves the focus, Cross presses, Circle closes. */
export function tourPadCommand(button: number): TourPadCommand | null {
  switch (button) {
    case 0:
      return "activate";
    case 1:
      return "close";
    case 12:
    case 14:
      return "prev";
    case 13:
    case 15:
      return "next";
    default:
      return null;
  }
}

export interface TourCursor {
  card: number;
  focus: number;
}

/** The cursor after a pad command or a button press; `done` once the tour is
 *  finished, skipped or closed (the caller marks it seen). */
export function tourStep(c: TourCursor, cmd: TourPadCommand | TourButton): TourCursor & { done: boolean } {
  const n = TOUR_BUTTONS.length;
  switch (cmd) {
    case "prev":
      return { ...c, focus: (c.focus - 1 + n) % n, done: false };
    case "next":
      return { ...c, focus: (c.focus + 1) % n, done: false };
    case "activate":
      return tourStep(c, TOUR_BUTTONS[c.focus] ?? "advance");
    case "close":
    case "skip":
      return { ...c, done: true };
    case "advance":
      return c.card >= TOUR_CARDS.length - 1 ? { ...c, done: true } : { card: c.card + 1, focus: 0, done: false };
  }
}

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** Space between the card and the screen edge or an avoided zone, px. */
const CARD_GAP_PX = 12;

/** Where the non-modal tour card sits in a `vw`×`vh` window: low over the
 *  video, right of the left column, never over the top bar, the alert stack
 *  or STOP, at most `maxW` wide. `h` is the card's natural height; `bottomPx`
 *  how high above the bottom edge it sits. When the room below the top zones
 *  is shorter than the card, the returned `h` is the room left (the card's
 *  body scrolls). */
export function tourCardRect(
  vw: number,
  vh: number,
  avoid: { leftColumn: Rect | null; stop: Rect | null; topBar: Rect | null; alertStack: Rect | null },
  maxW: number,
  h: number,
  bottomPx: number,
): Rect {
  const left = avoid.leftColumn ? avoid.leftColumn.x + avoid.leftColumn.w + CARD_GAP_PX : CARD_GAP_PX;
  const room = Math.max(0, vw - CARD_GAP_PX - left);
  const w = Math.min(maxW, room);
  const x = left + (room - w) / 2;
  // The top zones the card's columns run under push its top edge down.
  let top = CARD_GAP_PX;
  for (const z of [avoid.topBar, avoid.alertStack, avoid.stop]) {
    if (z && x < z.x + z.w && z.x < x + w) top = Math.max(top, z.y + z.h + CARD_GAP_PX);
  }
  const y = Math.max(top, vh - bottomPx - h);
  return { x, y, w, h: Math.max(0, Math.min(h, vh - CARD_GAP_PX - y)) };
}
