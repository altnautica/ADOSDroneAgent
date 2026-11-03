import { describe, expect, it } from "vitest";

import { rigRejection, sensorTone } from "@/lib/sensors-model";
import { ApiError } from "@/shared/api-fetch";
import { parseRigFile } from "@/shared/api/sensors";

describe("sensor stream tone", () => {
  it("colours a stream by its rate against the expected rate", () => {
    expect(sensorTone({ rate_hz: 199, expected_hz: 200 })).toBe("ok");
    expect(sensorTone({ rate_hz: 120, expected_hz: 200 })).toBe("warn");
    expect(sensorTone({ rate_hz: 20, expected_hz: 200 })).toBe("err");
    expect(sensorTone({ rate_hz: 20, expected_hz: null })).toBe("muted");
  });
});

describe("rig upload", () => {
  it("refuses a file that is not a JSON object before sending it", () => {
    expect(parseRigFile("{").ok).toBe(false);
    expect(parseRigFile("[1]")).toEqual({ ok: false, reason: "shape", detail: "" });
    expect(parseRigFile('{"rig_id":"r1"}')).toEqual({ ok: true, rig: { rig_id: "r1" } });
  });

  it("reads the agent's contract refusal", () => {
    const err = new ApiError("bad", 400, { detail: { error: { code: "E_RIG_INVALID", message: "missing sensors" } } });
    expect(rigRejection(err).code).toBe("E_RIG_INVALID");
    expect(rigRejection(new Error("offline"))).toEqual({ code: "?", detail: "offline" });
  });
});
