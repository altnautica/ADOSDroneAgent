import { describe, expect, it } from "vitest";

import { rigRejection } from "@/lib/sensors-model";
import { NO_RIG, formatOffset, pickRig, rigPayload } from "@/lib/settings/sensors";
import { ApiError } from "@/shared/api-fetch";

describe("rig pick gate", () => {
  it("arms the replace only for a parsed JSON object", () => {
    const ready = pickRig("rig.json", '{"rig_id":"r1","sensors":[]}');
    expect(ready).toEqual({ state: "ready", name: "rig.json", rig: { rig_id: "r1", sensors: [] } });
    expect(rigPayload(ready)).toEqual({ rig_id: "r1", sensors: [] });
  });

  it("sends nothing for a file that does not parse or is not an object", () => {
    const broken = pickRig("rig.json", "{nope");
    expect(broken.state).toBe("invalid");
    expect(broken.state === "invalid" && broken.reason).toBe("parse");
    expect(rigPayload(broken)).toBeNull();

    const list = pickRig("rig.json", "[1,2]");
    expect(list.state === "invalid" && list.reason).toBe("shape");
    expect(rigPayload(list)).toBeNull();

    expect(rigPayload(NO_RIG)).toBeNull();
  });

  it("reads the agent's contract refusal", () => {
    const err = new ApiError("400", 400, { error: "E_RIG_INVALID", detail: "sensors[0].kind missing" });
    expect(rigRejection(err)).toEqual({ code: "E_RIG_INVALID", detail: "sensors[0].kind missing" });
  });
});

describe("formatOffset", () => {
  it("keeps the sign of a clock offset", () => {
    expect(formatOffset(-2_500_000)).toBe("-2.50 ms");
    expect(formatOffset(4000)).toBe("4 µs");
  });
});
