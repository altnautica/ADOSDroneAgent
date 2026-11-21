import { describe, expect, it } from "vitest";

import { parsePilotState } from "./cockpit-frames";
import { followHeightLine, overheadAheadLine } from "./follow-height";
import { parseFollowState } from "./nav-types";
import { parseOverheadAhead } from "./pilot-fields";

describe("follow height under leaves", () => {
  it("parses the lowered height and its reason from follow.state", () => {
    const f = parseFollowState({ phase: "following", height_setpoint_m: 3.87, height_lowered: true, height_reason: "canopy" })!;
    expect(f).toMatchObject({ heightSetpointM: 3.87, heightLowered: true, heightReason: "canopy" });
    expect(followHeightLine(f)).toEqual({
      text: "Flying under leaves: 3.9 m",
      reason: "Lowered under the leaves ahead",
    });
  });

  it("names the floor reason and drops a reason it does not know", () => {
    const floor = parseFollowState({ phase: "following", height_setpoint_m: 3.4, height_lowered: true, height_reason: "canopy_floor" });
    expect(followHeightLine(floor)?.reason).toBe("At the lowest follow height, leaves lower still");
    const odd = parseFollowState({ phase: "following", height_setpoint_m: 4, height_lowered: true, height_reason: "fog" })!;
    expect(odd.heightReason).toBeNull();
    expect(followHeightLine(odd)).toEqual({ text: "Flying under leaves: 4.0 m", reason: null });
  });

  it("shows nothing while the height flown is the one set", () => {
    const f = parseFollowState({ phase: "following", height_setpoint_m: 6 })!;
    expect(f).toMatchObject({ heightLowered: false, heightReason: null });
    expect(followHeightLine(f)).toBeNull();
    expect(followHeightLine(parseFollowState({ phase: "following", height_lowered: true }))).toBeNull();
    expect(followHeightLine(null)).toBeNull();
  });
});

describe("overhead_ahead", () => {
  it("parses the pilot frame's report and words it", () => {
    const o = parseOverheadAhead({ lease_id: 7, fly_under_agl_m: 3.87, ahead_m: 2.5 });
    expect(o).toEqual({ leaseId: 7, flyUnderAglM: 3.87, aheadM: 2.5 });
    expect(overheadAheadLine(o)).toBe("Overhead clearance ahead: 3.9 m in 3 m");
    expect(parsePilotState({ owner: "skill", overhead_ahead: { lease_id: 7, fly_under_agl_m: 3.1, ahead_m: 12.2 } })?.overheadAhead).toEqual({
      leaseId: 7,
      flyUnderAglM: 3.1,
      aheadM: 12.2,
    });
  });

  it("is absent while nothing lies overhead, or when malformed", () => {
    expect(parseOverheadAhead({ lease_id: 7, fly_under_agl_m: null, ahead_m: null })).toBeNull();
    expect(parseOverheadAhead({ lease_id: 7 })).toBeNull();
    expect(parseOverheadAhead({ fly_under_agl_m: 3.1, ahead_m: 2 })).toBeNull();
    expect(parseOverheadAhead(undefined)).toBeNull();
    expect(parsePilotState({ owner: "skill" })?.overheadAhead).toBeNull();
    expect(overheadAheadLine(null)).toBeNull();
  });
});
