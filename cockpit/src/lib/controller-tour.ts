// The first-connect controller tour's state: which cards it walks through and
// what it shows for the current controller and the `tourSeen` flag.

export const TOUR_CARDS = ["sticks", "safety", "skills"] as const;

/** What the first-connect sheet shows: nothing, the "press any button"
 *  prompt (the browser announced a controller it has not yet reported), or
 *  the tour. */
export function tourStage(padReported: boolean, padAnnounced: boolean, seen: boolean): "none" | "press" | "tour" {
  if (seen) return "none";
  if (padReported) return "tour";
  return padAnnounced ? "press" : "none";
}
