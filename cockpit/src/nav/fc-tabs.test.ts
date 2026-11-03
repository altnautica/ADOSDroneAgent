import { describe, expect, it } from "vitest";

import { fcFamilyOf, tabVisibleFor } from "@/nav/navigator";
import { tabScreens } from "@/nav/registry";

const ids = (fc: "mavlink" | "msp" | null, profile: "drone" | "visor" | "ground_station" = "drone") =>
  tabScreens(profile, null, fc).map((s) => s.id);

describe("FC family", () => {
  it("names MSP from the family or the variant, MAVLink from ArduPilot/PX4", () => {
    expect(fcFamilyOf("betaflight", null)).toBe("msp");
    expect(fcFamilyOf("unknown", "iNav")).toBe("msp");
    expect(fcFamilyOf("ArduPilot", null)).toBe("mavlink");
    expect(fcFamilyOf("px4", null)).toBe("mavlink");
    expect(fcFamilyOf("unknown", null)).toBeNull();
    expect(fcFamilyOf(undefined, undefined)).toBeNull();
  });
});

describe("FC configuration tabs", () => {
  it("show both while the FC is not identified", () => {
    expect(ids(null)).toEqual(expect.arrayContaining(["params", "msp"]));
  });

  it("show Params only for a MAVLink FC and MSP only for an MSP FC", () => {
    expect(ids("mavlink")).toContain("params");
    expect(ids("mavlink")).not.toContain("msp");
    expect(ids("msp")).toContain("msp");
    expect(ids("msp")).not.toContain("params");
  });

  it("stay off profiles without a flight controller and leave other tabs alone", () => {
    expect(ids("mavlink", "ground_station")).not.toContain("params");
    expect(ids("msp")).toContain("feed");
    expect(tabVisibleFor({}, "drone", null, "msp")).toBe(true);
    expect(tabVisibleFor({ fcFamily: "mavlink" }, "unknown", null, "msp")).toBe(true);
  });
});
