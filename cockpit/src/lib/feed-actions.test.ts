import { describe, expect, it } from "vitest";

import { pushCommandHandler } from "@/lib/command-stack";
import { feedNavCommand, profileActionForKey } from "@/lib/feed-actions";
import { TAB_ORDER } from "@/nav/navigator";
import { useNavStore } from "@/stores/nav-store";

describe("profile keys", () => {
  it("maps F1–F3 to the profile actions once prefixed", () => {
    expect(profileActionForKey("F1")).toBe("profile:cine");
    expect(profileActionForKey("f2")).toBe("profile:normal");
    expect(profileActionForKey("f3")).toBe("profile:sport");
    expect(profileActionForKey("f4")).toBeNull();
  });
});

describe("panel buttons on the flight screen", () => {
  it("never step a hidden menu focus off the Feed; activate opens the Menu instead", () => {
    useNavStore.setState({
      activeTabId: "feed",
      detailStack: [],
      tabChosen: true,
      menuFocusIndex: 0,
      quickMenuOpen: false,
      visibleTabs: [...TAB_ORDER],
    });
    const off = pushCommandHandler(feedNavCommand);
    try {
      const nav = useNavStore.getState();
      nav.command("next");
      nav.command("prev");
      nav.command("next");
      nav.command("activate");
      const s = useNavStore.getState();
      expect(s.activeTabId).toBe("feed");
      expect(s.menuFocusIndex).toBe(0);
      expect(s.quickMenuOpen).toBe(true);
    } finally {
      off();
    }
  });
});
