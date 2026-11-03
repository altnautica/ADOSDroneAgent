import { beforeEach, describe, expect, it } from "vitest";

import { hasDrafts, useDirtyStore } from "@/stores/dirty-store";
import { useNavStore } from "@/stores/nav-store";

describe("dirty guard", () => {
  beforeEach(() => {
    useDirtyStore.setState({ owners: [], pending: null });
    useNavStore.setState({ activeTabId: "system", detailStack: [] });
  });

  it("tracks owners idempotently", () => {
    const s = useDirtyStore.getState();
    s.setDirty("wifi", true);
    s.setDirty("wifi", true);
    expect(useDirtyStore.getState().owners).toEqual(["wifi"]);
    s.setDirty("wifi", false);
    expect(hasDrafts()).toBe(false);
  });

  it("leaves at once when nothing is dirty", () => {
    let left = false;
    expect(useDirtyStore.getState().requestLeave(() => (left = true))).toBe(true);
    expect(left).toBe(true);
  });

  it("parks a leave while drafts exist, and discard runs it", () => {
    useDirtyStore.getState().setDirty("wifi", true);
    let left = false;
    expect(useDirtyStore.getState().requestLeave(() => (left = true))).toBe(false);
    expect(left).toBe(false);
    expect(useDirtyStore.getState().pending).not.toBeNull();
    useDirtyStore.getState().discardAndLeave();
    expect(left).toBe(true);
    expect(hasDrafts()).toBe(false);
  });

  it("keep editing drops the parked leave and keeps the drafts", () => {
    useDirtyStore.getState().setDirty("wifi", true);
    let left = false;
    useDirtyStore.getState().requestLeave(() => (left = true));
    useDirtyStore.getState().keepEditing();
    expect(left).toBe(false);
    expect(useDirtyStore.getState().pending).toBeNull();
    expect(hasDrafts()).toBe(true);
  });

  it("gates the navigator's back and tab switch", () => {
    useNavStore.setState({ activeTabId: "settings", detailStack: ["leaf"] });
    useDirtyStore.getState().setDirty("leaf", true);
    useNavStore.getState().back();
    expect(useNavStore.getState().detailStack).toEqual(["leaf"]);
    useDirtyStore.getState().discardAndLeave();
    expect(useNavStore.getState().detailStack).toEqual([]);

    useDirtyStore.getState().setDirty("settings", true);
    useNavStore.getState().goTab("system");
    expect(useNavStore.getState().activeTabId).toBe("settings");
    useDirtyStore.getState().discardAndLeave();
    expect(useNavStore.getState().activeTabId).toBe("system");
  });
});
