// The menu rail's state: one toggle cycles labelled → icons only → hidden →
// labelled, the choice persists per device under `ados-cockpit-rail`, and with
// nothing stored the kiosk starts icons only while a browser starts labelled.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const kiosk = vi.hoisted(() => ({ on: false }));
vi.mock("@/lib/kiosk", () => ({ isKiosk: () => kiosk.on }));

let stored: Map<string, string>;

beforeEach(() => {
  vi.resetModules();
  kiosk.on = false;
  stored = new Map();
  vi.stubGlobal("localStorage", {
    getItem: (k: string) => stored.get(k) ?? null,
    setItem: (k: string, v: string) => void stored.set(k, v),
    removeItem: (k: string) => void stored.delete(k),
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

// Imported per case after `vi.resetModules()`: the store reads the stored
// rail and the kiosk flag when its module loads.
const load = async () => (await import("@/stores/settings-store")).useSettingsStore;

describe("the menu rail state", () => {
  it("cycles labelled → compact → hidden → labelled", async () => {
    const store = await load();
    expect(store.getState().rail).toBe("labelled");
    const seen: string[] = [];
    for (let i = 0; i < 3; i++) {
      store.getState().cycleRail();
      seen.push(store.getState().rail);
    }
    expect(seen).toEqual(["compact", "hidden", "labelled"]);
  });

  it("persists the choice and reads it back on the next load", async () => {
    (await load()).getState().setRail("hidden");
    expect(stored.get("ados-cockpit-rail")).toBe("hidden");
    vi.resetModules();
    expect((await load()).getState().rail).toBe("hidden");
  });

  it("starts icons only on the kiosk and labelled in a browser, with nothing stored", async () => {
    kiosk.on = true;
    expect((await load()).getState().rail).toBe("compact");
    vi.resetModules();
    kiosk.on = false;
    expect((await load()).getState().rail).toBe("labelled");
  });

  it("ignores a stored value it does not know", async () => {
    stored.set("ados-cockpit-rail", "wide");
    kiosk.on = true;
    expect((await load()).getState().rail).toBe("compact");
  });
});
