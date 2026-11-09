// The first-connect controller tour's state: which cards it walks through and
// what it shows for the current controller, the `tourSeen` flag and whether
// the pilot is flying from the flight screen.

export const TOUR_CARDS = ["sticks", "safety", "skills"] as const;

/** What the first-connect sheet shows: nothing, the "press any button"
 *  prompt (the browser announced a controller it has not yet reported), or
 *  the tour. The sheet is modal and would cover STOP and the left column, so
 *  it waits while the flight screen is up with the vehicle armed or in the
 *  air, and opens once it is on the ground or the pilot leaves the screen. */
export function tourStage(padReported: boolean, padAnnounced: boolean, seen: boolean, flying: boolean): "none" | "press" | "tour" {
  if (seen || flying) return "none";
  if (padReported) return "tour";
  return padAnnounced ? "press" : "none";
}

/** Whether the tour must wait: the flight screen is up and the vehicle is
 *  armed or off the ground (the pilot kind, else the telemetry's arm state). */
export function tourDeferred(
  onFlightScreen: boolean,
  pilot: { armed: boolean; landed: boolean } | null,
  telemetryArmed: boolean | null | undefined,
): boolean {
  if (!onFlightScreen) return false;
  if (pilot) return pilot.armed || !pilot.landed;
  return telemetryArmed === true;
}
