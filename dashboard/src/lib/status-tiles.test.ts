import { describe, expect, it } from "vitest";

import { buildTiles, type TileInputs } from "./status-tiles";
import type { DashboardSnapshot, SetupStatus } from "./types";

function snapshot(fcConnected: boolean, pairingCode = ""): DashboardSnapshot {
  return {
    fc: { connected: fcConnected },
    cloud: { mode: "cloud", drone_id: "d1", pairing_code: pairingCode },
  } as unknown as DashboardSnapshot;
}

function tile(inputs: Partial<TileInputs>, label: string) {
  const all = buildTiles({
    snapshot: undefined,
    status: undefined,
    fcVariant: null,
    fcReachable: null,
    pairing: undefined,
    ...inputs,
  });
  const t = all.find((x) => x.label === label);
  if (!t) throw new Error(`no ${label} tile`);
  return t;
}

describe("PAIR tile", () => {
  const finalized = (v: boolean) => ({ setup_finalized: v }) as SetupStatus;

  it.each([
    // paired, finalized → shows paired regardless of the wizard flag
    [true, false, "paired", "linked"],
    [true, true, "paired", "linked"],
    // unpaired, finalized → never claims paired
    [false, true, "—", "unpaired"],
    [false, false, "—", "unpaired"],
  ])("paired=%s finalized=%s reads %s / %s", (paired, fin, value, sub) => {
    const t = tile(
      { snapshot: snapshot(false), status: finalized(fin), pairing: { paired, pairing_code: null } },
      "PAIR",
    );
    expect(t.value).toBe(value);
    expect(t.sub).toBe(sub);
  });

  it("shows the code while unpaired", () => {
    const t = tile(
      { snapshot: snapshot(false), pairing: { paired: false, pairing_code: "ABC123" } },
      "PAIR",
    );
    expect(t.value).toBe("ABC123");
    expect(t.sub).toBe("code rotates");
  });

  it("claims nothing before the pairing state is read", () => {
    const t = tile({ snapshot: snapshot(false), status: finalized(true) }, "PAIR");
    expect(t.value).toBe("—");
    expect(t.severity).toBe("idle");
  });
});

describe("FC tile (MSP)", () => {
  it("reads off when the variant persists but the FC is not reachable", () => {
    const t = tile({ snapshot: snapshot(false), fcVariant: "betaflight", fcReachable: false }, "FC");
    expect(t.value).toBe("off");
    expect(t.severity).toBe("idle");
    expect(t.sub).toBe("betaflight · msp");
  });

  it("reads linked only when the FC is reachable", () => {
    const t = tile({ snapshot: snapshot(false), fcVariant: "inav", fcReachable: true }, "FC");
    expect(t.value).toBe("linked");
    expect(t.severity).toBe("ok");
  });
});
