import { describe, expect, it } from "vitest";

import { resolveFirmwareType } from "./fc-firmware";

describe("resolveFirmwareType", () => {
  it("maps the agent's fixed-wing and VTOL labels to the plane catalog", () => {
    for (const v of ["Fixed Wing", "VTOL Quadrotor", "VTOL Tailsitter", "VTOL Tailsitter Duo", "VTOL Tiltrotor", "VTOL Tiltwing"]) {
      expect(resolveFirmwareType("ardupilot", v)).toBe("ardupilot-plane");
    }
  });

  it("maps rover, boat and submarine labels", () => {
    expect(resolveFirmwareType("ardupilot", "Ground Rover")).toBe("ardupilot-rover");
    expect(resolveFirmwareType("ardupilot", "Surface Boat")).toBe("ardupilot-rover");
    expect(resolveFirmwareType("ardupilot", "Submarine")).toBe("ardupilot-sub");
  });

  it("defaults multirotors and unknown vehicles to copter", () => {
    expect(resolveFirmwareType("ardupilot", "Quadrotor")).toBe("ardupilot-copter");
    expect(resolveFirmwareType("ardupilot", "Hexarotor")).toBe("ardupilot-copter");
    expect(resolveFirmwareType("ardupilot", null)).toBe("ardupilot-copter");
  });

  it("passes non-ArduPilot families through", () => {
    expect(resolveFirmwareType("px4", "Fixed Wing")).toBe("px4");
    expect(resolveFirmwareType("betaflight", null)).toBe("betaflight");
    expect(resolveFirmwareType(null, null)).toBe("unknown");
  });
});
