import { beforeEach, describe, expect, it } from "vitest";

import { TAB_ORDER } from "@/nav/navigator";
import { useNavStore } from "@/stores/nav-store";

/** The tabs a drone shows: the ground-station-only screens are hidden. */
const DRONE_TABS = TAB_ORDER.filter((id) => id !== "mesh" && id !== "uplink");

function reset(tabs: string[] = [...TAB_ORDER], active = tabs[0]) {
  useNavStore.setState({
    activeTabId: active,
    detailStack: [],
    menuFocusIndex: 0,
    quickMenuOpen: false,
    menuCollapsed: false,
    visibleTabs: tabs,
  });
}

describe("navigating with the panel's own buttons", () => {
  beforeEach(() => reset());

  it("never steps onto a tab this profile hides", () => {
    // The bug: stepping walked the full registry order, so a drone's next/
    // activate buttons could land on a ground-station-only screen — one with no
    // menu entry, polling routes that 404 on this profile, which the operator
    // did not choose and cannot see a way back from.
    reset(DRONE_TABS);
    const seen = new Set<string>();
    for (let i = 0; i < TAB_ORDER.length * 2; i += 1) {
      useNavStore.getState().command("cycle-tab");
      seen.add(useNavStore.getState().activeTabId);
    }
    for (const id of seen) {
      expect(DRONE_TABS).toContain(id);
    }
    expect(seen.has("mesh")).toBe(false);
    expect(seen.has("uplink")).toBe(false);
  });

  it("activates the focused tab from the visible set", () => {
    reset(DRONE_TABS);
    useNavStore.getState().command("next");
    const focused = useNavStore.getState().menuFocusIndex;
    useNavStore.getState().command("activate");
    expect(useNavStore.getState().activeTabId).toBe(DRONE_TABS[focused]);
  });

  it("wraps the focus ring within the visible set, not the full one", () => {
    reset(DRONE_TABS);
    for (let i = 0; i < DRONE_TABS.length; i += 1) {
      useNavStore.getState().command("next");
    }
    // A full lap returns to the start; walking the unfiltered length would not.
    expect(useNavStore.getState().menuFocusIndex).toBe(0);
  });

  it("steps to the first visible tab when the active one is not in the set", () => {
    // A profile resolving while a now-hidden tab is open must not strand the
    // operator on a tab that stepping cannot leave.
    reset(DRONE_TABS, "mesh");
    useNavStore.getState().command("cycle-tab");
    expect(DRONE_TABS).toContain(useNavStore.getState().activeTabId);
  });

  it("does nothing rather than crashing when no tab is visible", () => {
    reset([], "feed");
    for (const cmd of ["next", "prev", "activate", "cycle-tab"] as const) {
      expect(() => useNavStore.getState().command(cmd)).not.toThrow();
    }
  });
});

describe("declaring the visible tabs", () => {
  beforeEach(() => reset());

  it("keeps the focus on the same tab when the set narrows", () => {
    // The profile probe resolves after the first render, so the set narrows
    // mid-session. The operator's selection must not silently move.
    reset([...TAB_ORDER]);
    const settingsIndex = TAB_ORDER.indexOf("settings");
    useNavStore.getState().setMenuFocus(settingsIndex);
    useNavStore.getState().setVisibleTabs(DRONE_TABS);
    const s = useNavStore.getState();
    expect(s.visibleTabs[s.menuFocusIndex]).toBe("settings");
  });

  it("falls back to the active tab when the focused one disappears", () => {
    reset([...TAB_ORDER], "feed");
    useNavStore.getState().setMenuFocus(TAB_ORDER.indexOf("mesh"));
    useNavStore.getState().setVisibleTabs(DRONE_TABS);
    const s = useNavStore.getState();
    expect(s.visibleTabs[s.menuFocusIndex]).toBe("feed");
  });

  it("does not churn state when the set is unchanged", () => {
    // Called from a render path, so an unconditional set would re-render every
    // subscriber on each pass.
    useNavStore.getState().setVisibleTabs([...TAB_ORDER]);
    const first = useNavStore.getState();
    useNavStore.getState().setVisibleTabs([...TAB_ORDER]);
    expect(useNavStore.getState()).toBe(first);
  });

  it("refuses a focus index outside the visible set", () => {
    reset(DRONE_TABS);
    useNavStore.getState().setMenuFocus(DRONE_TABS.length + 3);
    expect(useNavStore.getState().menuFocusIndex).toBe(0);
  });
});
