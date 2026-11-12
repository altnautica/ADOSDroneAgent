import { beforeEach, describe, expect, it } from "vitest";

import { TAB_ORDER } from "@/nav/screen-table";
import { useDirtyStore } from "@/stores/dirty-store";
import { activeScreenId, useNavStore } from "@/stores/nav-store";

describe("System detail screens", () => {
  beforeEach(() => {
    useDirtyStore.setState({ owners: [], pending: null });
    useNavStore.setState({ activeTabId: "system", detailStack: [] });
  });

  it("are not menu tabs", () => {
    expect(TAB_ORDER).not.toContain("peripherals");
    expect(TAB_ORDER).not.toContain("logs");
  });

  for (const id of ["peripherals", "logs"]) {
    it(`opens ${id} over System with open-detail and closes it with back`, () => {
      useNavStore.getState().dispatch({ kind: "open-detail", id });
      expect(activeScreenId(useNavStore.getState())).toBe(id);
      expect(useNavStore.getState().activeTabId).toBe("system");
      useNavStore.getState().dispatch({ kind: "back" });
      expect(activeScreenId(useNavStore.getState())).toBe("system");
    });
  }

  it("closes with the controller back command too", () => {
    useNavStore.getState().dispatch({ kind: "open-detail", id: "logs" });
    useNavStore.getState().command("back");
    expect(useNavStore.getState().detailStack).toEqual([]);
  });
});
