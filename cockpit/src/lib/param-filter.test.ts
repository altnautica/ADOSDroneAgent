import { beforeEach, describe, expect, it } from "vitest";

import { filterByNames, openParameters, PARAMS_TAB_ID, useParamFilterStore } from "@/lib/param-filter";
import { useNavStore } from "@/stores/nav-store";

const rows = ["FLOW_TYPE", "RNGFND1_TYPE", "SERIAL3_PROTOCOL", "MAV_GCS_SYSID"].map((name) => ({ name }));

beforeEach(() => useParamFilterStore.getState().clear());

describe("the Parameters name filter", () => {
  it("opens the Parameters tab showing only the named rows, and Clear restores every row", () => {
    openParameters(["FLOW_TYPE", "RNGFND1_TYPE"]);
    expect(useNavStore.getState().activeTabId).toBe(PARAMS_TAB_ID);
    const names = useParamFilterStore.getState().names;
    expect(filterByNames(rows, names).map((r) => r.name)).toEqual(["FLOW_TYPE", "RNGFND1_TYPE"]);
    useParamFilterStore.getState().clear();
    expect(filterByNames(rows, useParamFilterStore.getState().names)).toEqual(rows);
  });

  it("matches names exactly and treats an empty list as no filter", () => {
    expect(filterByNames(rows, ["FLOW"])).toEqual([]);
    openParameters([]);
    expect(useParamFilterStore.getState().names).toBeNull();
  });
});
