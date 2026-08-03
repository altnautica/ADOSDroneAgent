import { describe, expect, it } from "vitest";

import { adapterDetail } from "@/components/screens/link-screen";

// The Link tab crashed the instant it was opened: "Minified React error #31 —
// objects are not valid as a React child", with the offending keys named in the
// message as {chipset, driver, supports_monitor}. The agent sends `adapter` as
// an object; this screen had it hand-typed as a string and rendered it directly.
// Nothing checked that the hand-written type matched the wire.
describe("adapterDetail", () => {
  it("renders the object the agent actually sends, rather than throwing", () => {
    // The exact shape from GET /api/wfb on a live ground station.
    expect(
      adapterDetail({ chipset: "RTL8812EU", driver: "8812eu", supports_monitor: true } as never),
    ).toBe("8812eu · RTL8812EU");
  });

  it("returns nothing when every field is blank", () => {
    // A ground station whose radio has not been probed yet sends empty strings
    // for all of them. Joining those yields a stray " · " that tells an operator
    // nothing and looks like a rendering fault.
    expect(adapterDetail({ chipset: "", driver: "", supports_monitor: false } as never)).toBeUndefined();
  });

  it("uses whichever single field is present", () => {
    expect(adapterDetail({ driver: "8812eu" })).toBe("8812eu");
    expect(adapterDetail({ chipset: "RTL8812EU" })).toBe("RTL8812EU");
  });

  it("still accepts a plain string, in case an older agent sends one", () => {
    expect(adapterDetail("RTL8812EU")).toBe("RTL8812EU");
    expect(adapterDetail("   ")).toBeUndefined();
  });

  it("returns nothing for absent or unexpected values", () => {
    // The contract this function exists to enforce: a React child gets a string
    // or nothing, never an object, whatever the wire sends.
    expect(adapterDetail(null)).toBeUndefined();
    expect(adapterDetail(undefined)).toBeUndefined();
    expect(adapterDetail(42 as never)).toBeUndefined();
    expect(adapterDetail([] as never)).toBeUndefined();
  });
});
