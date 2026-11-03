import { describe, expect, it, vi } from "vitest";

import {
  capMbToGb,
  cellularGuard,
  changeSavedKey,
  configLeaf,
  modemFacts,
  parseCapGb,
  presenceKey,
  submitCellular,
  type CellularChange,
} from "./cellular";

describe("data cap", () => {
  it("formats MB as GB and parses a non-negative GB value", () => {
    expect(capMbToGb(2048)).toBe("2");
    expect(capMbToGb(0)).toBe("");
    expect(capMbToGb(null)).toBe("");
    expect(parseCapGb(" 1.5 ")).toBe(1.5);
    expect(parseCapGb("0")).toBe(0);
    expect(parseCapGb("")).toBeNull();
    expect(parseCapGb("-1")).toBeNull();
    expect(parseCapGb("lots")).toBeNull();
  });
});

describe("modem read", () => {
  it("reads presence reasons and treats the no-modem sentinels as unknown", () => {
    expect(presenceKey(null)).toBe("settings.shared.unknown");
    expect(presenceKey({ present: true })).toBe("settings.cellular.presenceDetected");
    expect(presenceKey({ present: false, reason: "no_modem" })).toBe("settings.cellular.presenceNoModem");
    expect(presenceKey({ present: false, reason: "not_probed" })).toBe("settings.shared.unknown");
    expect(modemFacts({ signal_quality: -1, technology: "unknown", operator: "", state: "" })).toMatchObject({
      signalQuality: null,
      technology: null,
      operator: null,
      state: null,
    });
  });
});

describe("cellular gate", () => {
  it("slides a disable and an APN change; writes an enable and a cap at once", () => {
    expect(cellularGuard({ enabled: false })).toBe("slide");
    expect(cellularGuard({ apn: "internet" })).toBe("slide");
    expect(cellularGuard({ enabled: true })).toBe("direct");
    expect(cellularGuard({ cap_gb: 5 })).toBe("direct");
  });

  it("never writes a slide-guarded change on submit", () => {
    const write = vi.fn();
    const stage = vi.fn();
    const guarded: CellularChange[] = [{ enabled: false }, { apn: "" }];
    for (const change of guarded) submitCellular(change, { write, stage });
    expect(write).not.toHaveBeenCalled();
    expect(stage.mock.calls).toEqual([[{ enabled: false }], [{ apn: "" }]]);

    submitCellular({ enabled: true }, { write, stage });
    expect(write).toHaveBeenCalledWith({ enabled: true });
    expect(stage).toHaveBeenCalledTimes(2);
  });
});

describe("config branch", () => {
  it("maps changes to the cellular config leaves; the cap has none", () => {
    expect(configLeaf({ enabled: false })).toEqual({ key: "network.cellular.enabled", value: false });
    expect(configLeaf({ apn: "iot" })).toEqual({ key: "network.cellular.apn", value: "iot" });
    expect(configLeaf({ cap_gb: 1 })).toBeNull();
    expect(changeSavedKey({ enabled: true }, false)).toBe("settings.cellular.cellularEnabled");
    expect(changeSavedKey({ enabled: true }, true)).toBe("settings.cellular.modemEnabled");
    expect(changeSavedKey({ cap_gb: 0 }, true)).toBe("settings.cellular.capCleared");
  });
});
