import { afterEach, describe, expect, it } from "vitest";

import { loadSnoozed, pruneSnoozed, saveSnoozed, SNOOZE_STORAGE_KEY, withSnoozed } from "./alert-snooze";

describe("alert snooze", () => {
  it("prunes keys whose alert cleared, so it shows again when it returns", () => {
    const s = new Set(["fcst:EKF variance|EKF variance", "pilot:flow|No flow"]);
    const next = pruneSnoozed(s, ["pilot:flow|No flow", "storage|Storage 91 % used"]);
    expect([...next]).toEqual(["pilot:flow|No flow"]);
    // Nothing pruned: the same set, so the caller skips a write.
    expect(pruneSnoozed(next, ["pilot:flow|No flow"])).toBe(next);
    expect([...pruneSnoozed(next, [])]).toEqual([]);
  });

  it("snoozes and shows again without touching the given set", () => {
    const s = new Set(["a"]);
    expect([...withSnoozed(s, "b", true)]).toEqual(["a", "b"]);
    expect([...withSnoozed(s, "a", false)]).toEqual([]);
    expect([...s]).toEqual(["a"]);
  });

  describe("storage", () => {
    const store = new Map<string, string>();
    Object.assign(globalThis, {
      localStorage: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => void store.set(k, v),
        removeItem: (k: string) => void store.delete(k),
      },
    });
    afterEach(() => store.clear());

    it("round-trips the keys and ignores anything but a string array", () => {
      saveSnoozed(new Set(["x|y"]));
      expect(store.get(SNOOZE_STORAGE_KEY)).toBe('["x|y"]');
      expect([...loadSnoozed()]).toEqual(["x|y"]);
      store.set(SNOOZE_STORAGE_KEY, '{"x":1}');
      expect([...loadSnoozed()]).toEqual([]);
      store.set(SNOOZE_STORAGE_KEY, "not json");
      expect([...loadSnoozed()]).toEqual([]);
      store.set(SNOOZE_STORAGE_KEY, '["a", 3, "b"]');
      expect([...loadSnoozed()]).toEqual(["a", "b"]);
    });
  });
});
