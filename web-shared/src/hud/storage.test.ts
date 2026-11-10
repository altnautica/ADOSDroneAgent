import { describe, expect, it } from "vitest";

import { storagePct } from "./storage";

describe("storagePct", () => {
  it("reads a drone's health.disk_percent first", () => {
    expect(storagePct({ health: { disk_percent: 98 }, system: { disk_pct: 10 } })).toBe(98);
  });
  it("falls back to a ground station's system.disk_pct", () => {
    expect(storagePct({ health: {}, system: { disk_pct: 97 } })).toBe(97);
  });
  it("is null when neither is reported", () => {
    expect(storagePct(null)).toBeNull();
    expect(storagePct({ health: { disk_percent: null } })).toBeNull();
  });
});
