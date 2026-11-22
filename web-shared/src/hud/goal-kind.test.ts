import { describe, expect, it } from "vitest";

import { parsePilotState } from "./cockpit-frames";
import { goalKindChipText } from "./goal-kind";
import { parseFollowView, parseGoalKind } from "./pilot-fields";

describe("goal chip", () => {
  it("words a follow intent with the viewpoint the pilot picked", () => {
    const view = parseFollowView({ bearing_deg: 15.4, radius_m: 7.6, height_m: 4.5, visibility: 0.9 })!;
    expect(view).toEqual({ bearingDeg: 15.4, radiusM: 7.6, heightM: 4.5, visibility: 0.9 });
    expect(goalKindChipText("follow_intent", view)).toBe("Follow intent · view 15° 8 m");
    expect(goalKindChipText("follow_intent", parseFollowView({ bearing_deg: 359.7, radius_m: 6, height_m: 4, visibility: 1 }))).toBe(
      "Follow intent · view 0° 6 m",
    );
    // No viewpoint picked yet: the kind alone.
    expect(goalKindChipText("follow_intent", null)).toBe("Follow intent");
  });

  it("labels every other kind", () => {
    expect(goalKindChipText("follow", null)).toBe("Ring");
    expect(goalKindChipText("orbit", null)).toBe("Orbit");
    expect(goalKindChipText("position", null)).toBe("Position");
    expect(goalKindChipText("vantage", null)).toBe("Vantage");
  });

  it("shows nothing with no goal kind, or one this surface does not know", () => {
    expect(parseGoalKind(undefined)).toBeNull();
    expect(parseGoalKind("wander")).toBeNull();
    expect(parseFollowView({ bearing_deg: 15, radius_m: 8 })).toBeNull();
    expect(goalKindChipText(null, null)).toBeNull();
  });

  it("rides on the pilot state", () => {
    const base = { owner: "skill", fc_mode: 4, armed: true };
    const none = parsePilotState(base);
    expect([none?.goalKind, none?.view]).toEqual([null, null]);
    const p = parsePilotState({
      ...base,
      goal_kind: "follow_intent",
      view: { bearing_deg: 180, radius_m: 6, height_m: 4.2, visibility: 0.75 },
    });
    expect(p?.goalKind).toBe("follow_intent");
    expect(p?.view).toEqual({ bearingDeg: 180, radiusM: 6, heightM: 4.2, visibility: 0.75 });
  });
});
