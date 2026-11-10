// DS4 Options does what `ds4.json` binds it to, and on every surface that is
// the Menu.

import { afterEach, describe, expect, it, vi } from "vitest";

import { runCockpitAction } from "@/lib/cockpit-actions";
import { registerFeedActions } from "@/lib/feed-actions";
import { DEFAULT_BINDINGS } from "@/lib/pad/ds4-map";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useNavStore } from "@/stores/nav-store";

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("DS4 Options", () => {
  it("opens the Menu through the action ds4.json binds it to", () => {
    vi.stubGlobal("window", { addEventListener: () => {}, removeEventListener: () => {} });
    const off = registerFeedActions();
    const options = DEFAULT_BINDINGS.find((b) => b.control === "options" && b.with.length === 0 && b.gesture === "tap");
    expect(options).toBeDefined();
    useNavStore.setState({ quickMenuOpen: false });
    expect(runCockpitAction(options!.action)).toBe(true);
    expect(useNavStore.getState().quickMenuOpen).toBe(true);
    expect(useCockpitUiStore.getState().sheet).toBeNull();
    off();
  });
});
