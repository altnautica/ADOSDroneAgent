import { beforeEach, describe, expect, it } from "vitest";

import { useCockpitViewStore } from "@/stores/cockpit-view-store";

const mem = new Map<string, string>();
globalThis.localStorage = {
  getItem: (k: string) => mem.get(k) ?? null,
  setItem: (k: string, v: string) => void mem.set(k, v),
  removeItem: (k: string) => void mem.delete(k),
  clear: () => mem.clear(),
  key: () => null,
  length: 0,
} as Storage;

// The store reads storage when a vehicle binds, so the stand-in above is in
// place before any test runs.

describe("the cockpit view store", () => {
  beforeEach(() => mem.clear());

  it("cycles pilot → map → split → vision for the Menu's View entry", () => {
    useCockpitViewStore.getState().bindVehicle("cycle", "full");
    useCockpitViewStore.getState().setView("pilot");
    const seen: string[] = [];
    for (let i = 0; i < 4; i += 1) {
      useCockpitViewStore.getState().cycleView();
      seen.push(useCockpitViewStore.getState().view);
    }
    expect(seen).toEqual(["map", "split", "vision", "pilot"]);
  });

  it("remembers the split divider per vehicle with the view, clamped to 0.3–0.7", () => {
    const s = () => useCockpitViewStore.getState();
    s().bindVehicle("drone-a", "full");
    expect(s().split).toBe(0.5);
    s().setSplit(0.9);
    expect(s().split).toBe(0.7);
    s().setSplit(0.4);
    s().bindVehicle("drone-b", "full");
    expect(s().split).toBe(0.5);
    s().bindVehicle("drone-a", "full");
    expect(s().split).toBe(0.4);
    expect(JSON.parse(mem.get("ados-cockpit-view:drone-a") ?? "{}").split).toBe(0.4);
  });
});
