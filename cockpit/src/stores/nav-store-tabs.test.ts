import { beforeEach, describe, expect, it } from "vitest";

import { allScreens } from "@/nav/registry";
import { useDirtyStore } from "@/stores/dirty-store";
import { useNavStore } from "@/stores/nav-store";

const REGISTERED_TABS = allScreens()
  .filter((s) => s.kind === "tab")
  .map((s) => s.id);

describe("goTab", () => {
  beforeEach(() => {
    useDirtyStore.setState({ owners: [], pending: null });
    useNavStore.setState({
      activeTabId: "feed",
      detailStack: [],
      tabChosen: false,
      menuFocusIndex: 0,
      quickMenuOpen: false,
      visibleTabs: [...REGISTERED_TABS],
    });
  });

  it.each(REGISTERED_TABS)("reaches the registered tab %s", (id) => {
    useNavStore.getState().goTab(id);
    expect(useNavStore.getState().activeTabId).toBe(id);
  });

  it("ignores an id that is not a registered tab", () => {
    useNavStore.getState().goTab("logs");
    useNavStore.getState().goTab("nope");
    expect(useNavStore.getState().activeTabId).toBe("feed");
  });
});
