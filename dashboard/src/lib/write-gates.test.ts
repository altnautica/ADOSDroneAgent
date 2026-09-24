import { describe, expect, it } from "vitest";

import { armedWriteRefusal, inavSaveOutcome } from "./msp/fc-settings";
import { paramWriteBlock } from "./params";

describe("parameter write gate", () => {
  it("allows a write only when the FC reports disarmed", () => {
    expect(paramWriteBlock(false)).toBeNull();
    expect(paramWriteBlock(true)).toMatch(/armed/i);
  });

  it("blocks a write while the armed state has not been reported", () => {
    // The agent sends null until the heartbeat reports armed state; treating
    // that as disarmed let writes reach a possibly armed vehicle.
    expect(paramWriteBlock(null)).not.toBeNull();
    expect(paramWriteBlock(undefined)).not.toBeNull();
  });
});

describe("MSP settings write gate", () => {
  it("refuses unless the FC itself reports disarmed", () => {
    expect(armedWriteRefusal(false)).toBeNull();
    expect(armedWriteRefusal(true)).not.toBeNull();
    expect(armedWriteRefusal(null)).not.toBeNull();
  });
});

describe("iNav save outcome", () => {
  it("does not call a RAM-only change saved when the EEPROM write failed", () => {
    const res = inavSaveOutcome([], "MSP error for command 250");
    expect(res.ok).toBe(false);
    expect(res.message).toMatch(/EEPROM/);
  });

  it("reports rejected settings and a clean save", () => {
    expect(inavSaveOutcome(["nav_rth_altitude"], null).ok).toBe(false);
    expect(inavSaveOutcome([], null)).toEqual({ ok: true, message: "Saved to EEPROM" });
  });
});
