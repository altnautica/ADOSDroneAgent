// The menu rail's state: one toggle flips labelled ↔ hidden, the choice
// persists per device under `ados-cockpit-rail`, and with nothing stored the
// rail starts labelled everywhere, the kiosk included.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

let stored: Map<string, string>;

beforeEach(() => {
  vi.resetModules();
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
// rail when its module loads.
const load = async () => (await import("@/stores/settings-store")).useSettingsStore;

describe("the menu rail state", () => {
  it("flips labelled → hidden → labelled", async () => {
    const store = await load();
    expect(store.getState().rail).toBe("labelled");
    const seen: string[] = [];
    for (let i = 0; i < 2; i++) {
      store.getState().cycleRail();
      seen.push(store.getState().rail);
    }
    expect(seen).toEqual(["hidden", "labelled"]);
  });

  it("persists the choice and reads it back on the next load", async () => {
    (await load()).getState().setRail("hidden");
    expect(stored.get("ados-cockpit-rail")).toBe("hidden");
    vi.resetModules();
    expect((await load()).getState().rail).toBe("hidden");
  });

  it("reads a stored icons-only state from the old three-state rail as labelled", async () => {
    stored.set("ados-cockpit-rail", "compact");
    expect((await load()).getState().rail).toBe("labelled");
  });

  it("starts labelled with nothing stored or a value it does not know", async () => {
    expect((await load()).getState().rail).toBe("labelled");
    vi.resetModules();
    stored.set("ados-cockpit-rail", "wide");
    expect((await load()).getState().rail).toBe("labelled");
  });
});
