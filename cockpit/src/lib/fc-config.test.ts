import { describe, expect, it } from "vitest";

import {
  appendTranscript,
  cliCommandRisk,
  displayValue,
  mspSaveBlock,
  documentedMask,
  identityFrom,
  paramHint,
  parseEntry,
  saveParamDrafts,
  setDocumented,
  toggleBit,
  undocumentedBits,
  useParamDrafts,
  withDraft,
  withMspDraft,
  type ParamDraft,
  type ParamSetResponse,
} from "@/lib/fc-config";
import { armedWriteRefusal } from "@/shared/api/msp/fc-settings";
import type { ParamMetadata } from "@/shared/api/param-metadata";
import { paramWriteBlock } from "@/shared/api/params";

describe("FC identity", () => {
  it("routes an ArduPilot copter to the MAVLink params and reads armed", () => {
    expect(identityFrom({ fcFirmware: "ardupilot" }, { vehicle: "Quadrotor", firmware: "4.6.0", armed: false })).toEqual({
      firmwareType: "ardupilot-copter",
      msp: null,
      firmwareVersion: "4.6.0",
      armed: false,
      mspArmed: null,
    });
  });

  it("routes a Betaflight FC to MSP, from the family or the variant", () => {
    expect(identityFrom({ fcFirmware: "betaflight" }, null).msp).toBe("betaflight");
    const v = identityFrom({ fcFirmware: "unknown", fcVariant: "inav" }, null);
    expect(v.msp).toBe("inav");
    expect(v.firmwareType).toBe("inav");
  });

  it("keeps an unreported armed state unknown, never disarmed", () => {
    expect(identityFrom({ fcFirmware: "px4" }, { armed: null }).armed).toBeNull();
    expect(identityFrom(null, null).armed).toBeNull();
  });
});

describe("write gates", () => {
  it("only a reported-disarmed FC takes parameter writes", () => {
    expect(paramWriteBlock(false)).toBeNull();
    expect(paramWriteBlock(true)).toMatch(/armed/i);
    expect(paramWriteBlock(null)).not.toBeNull();
    expect(paramWriteBlock(undefined)).not.toBeNull();
  });

  it("the MSP save refuses an armed or unreadable FC", () => {
    expect(armedWriteRefusal(false)).toBeNull();
    expect(armedWriteRefusal(true)).not.toBeNull();
    expect(armedWriteRefusal(null)).not.toBeNull();
  });
});

describe("parameter drafts", () => {
  it("drops a draft edited back to the original", () => {
    let d: Map<string, ParamDraft> = withDraft(new Map(), "BATT_LOW_VOLT", 10.5, 11);
    expect(d.get("BATT_LOW_VOLT")).toEqual({ original: 10.5, draft: 11 });
    d = withDraft(d, "BATT_LOW_VOLT", 10.5, 10.5);
    expect(d.size).toBe(0);
  });

  it("the session store tracks and clears drafts", () => {
    const s = useParamDrafts.getState();
    s.setDraft("A_B", 1, 2);
    s.setDraft("C_D", 1, 3);
    expect(useParamDrafts.getState().drafts.size).toBe(2);
    useParamDrafts.getState().discardDraft("A_B");
    expect([...useParamDrafts.getState().drafts.keys()]).toEqual(["C_D"]);
    useParamDrafts.getState().clearAll();
    expect(useParamDrafts.getState().drafts.size).toBe(0);
  });

  it("saves one at a time, sorting acked, pending and failed, and keeps going", async () => {
    const drafts = new Map<string, ParamDraft>([
      ["A", { original: 0, draft: 1 }],
      ["B", { original: 0, draft: 2 }],
      ["C", { original: 0, draft: 3 }],
    ]);
    const order: string[] = [];
    let inFlight = 0;
    const progress: number[] = [];
    const post = async (name: string, value: number): Promise<ParamSetResponse> => {
      inFlight += 1;
      expect(inFlight).toBe(1);
      order.push(`${name}=${value}`);
      await Promise.resolve();
      inFlight -= 1;
      if (name === "B") throw new Error("timeout");
      return { name, value, ack: name === "A", cached_value: value, message: "" };
    };
    const out = await saveParamDrafts(drafts, post, (done) => progress.push(done));
    expect(order).toEqual(["A=1", "B=2", "C=3"]);
    expect(out).toEqual({ acked: ["A"], pending: ["C"], failed: [{ name: "B", message: "timeout" }] });
    expect(progress).toEqual([1, 2, 3]);
  });
});

describe("display", () => {
  const meta = (m: Partial<ParamMetadata>): ParamMetadata => ({ name: "X", humanName: "", description: "", ...m });

  it("labels enum codes and set bits", () => {
    expect(displayValue(1, meta({ values: new Map([[1, "Enabled"]]) }))).toBe("1 · Enabled");
    expect(displayValue(5, meta({ bitmask: new Map([[0, "GPS"], [1, "Baro"], [2, "Compass"]]) }))).toBe("5 · GPS, Compass");
    expect(displayValue(1.23456)).toBe("1.2346");
  });

  it("hints range and units", () => {
    expect(paramHint(meta({ range: { min: 0, max: 100 }, units: "%" }))).toBe("0…100 %");
    expect(paramHint(meta({ units: "m" }))).toBe("m");
    expect(paramHint()).toBe("");
  });

  it("parses only finite typed numbers", () => {
    expect(parseEntry("2.5")).toBe(2.5);
    expect(parseEntry(" ")).toBeNull();
    expect(parseEntry("abc")).toBeNull();
  });
});

describe("bitmasks", () => {
  const bm = new Map([
    [0, "a"],
    [2, "c"],
  ]);
  it("toggles single bits and documented groups, preserving undocumented ones", () => {
    expect(documentedMask(bm)).toBe(0b101);
    expect(toggleBit(0, 2, true)).toBe(4);
    expect(toggleBit(7, 1, false)).toBe(5);
    expect(setDocumented(0b1010, bm, true)).toBe(0b1111);
    expect(setDocumented(0b1111, bm, false)).toBe(0b1010);
    expect(undocumentedBits(0b1011, bm)).toBe(0b1010);
    expect(toggleBit(0, 31, true)).toBe(2 ** 31);
  });
});

describe("MSP", () => {
  it("reads the agent's MSP armed flag and disables save only when armed", () => {
    expect(identityFrom({ fcFirmware: "betaflight", fcMspArmed: true }, null).mspArmed).toBe(true);
    expect(identityFrom({ fcFirmware: "inav", fcMspArmed: null }, null).mspArmed).toBeNull();
    expect(mspSaveBlock(true)).toMatch(/armed/i);
    expect(mspSaveBlock(false)).toBeNull();
    // Unknown stays enabled: the client re-reads the FC at save and refuses then.
    expect(mspSaveBlock(null)).toBeNull();
  });

  it("drops a draft equal to the FC value", () => {
    const d = withMspDraft(new Map(), "gyro_lpf1_static_hz", "200", "250");
    expect(d.get("gyro_lpf1_static_hz")).toBe("200");
    expect(withMspDraft(d, "gyro_lpf1_static_hz", "250", "250").size).toBe(0);
  });

  it("flags CLI lines that reboot or wipe the FC for the guarded control", () => {
    expect(cliCommandRisk("save")).toBe("reboot");
    expect(cliCommandRisk("  Exit ")).toBe("reboot");
    expect(cliCommandRisk("defaults nosave")).toBe("reboot");
    expect(cliCommandRisk("bl")).toBe("reboot");
    expect(cliCommandRisk("save noreboot")).toBe("none");
    expect(cliCommandRisk("get gyro")).toBe("none");
    expect(cliCommandRisk("constructor")).toBe("none");
    expect(cliCommandRisk("")).toBe("none");
  });

  it("bounds the CLI transcript at a line boundary", () => {
    expect(appendTranscript("ab\n", "cd\n", 100)).toBe("ab\ncd\n");
    expect(appendTranscript("line1\nline2\n", "line3\n", 10)).toBe("line3\n");
  });
});
