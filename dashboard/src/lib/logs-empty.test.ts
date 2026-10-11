import { describe, expect, it } from "vitest";

import { logsEmptyState, STORE_UNAVAILABLE } from "./logs-empty";

describe("logsEmptyState", () => {
  it("reports the store as off when the route says it is unavailable", () => {
    expect(logsEmptyState({ loading: false, filtered: false, warning: STORE_UNAVAILABLE })).toEqual({
      kind: "storeOff",
    });
  });

  it("reports the store as off even with filters set", () => {
    expect(logsEmptyState({ loading: false, filtered: true, warning: STORE_UNAVAILABLE })).toEqual({
      kind: "storeOff",
    });
  });

  it("surfaces any other store warning verbatim", () => {
    expect(
      logsEmptyState({ loading: false, filtered: false, warning: "logging store returned 500" }),
    ).toEqual({ kind: "storeError", warning: "logging store returned 500" });
  });

  it("keeps the empty and no-match states when the store answered", () => {
    expect(logsEmptyState({ loading: false, filtered: false, warning: undefined })).toEqual({
      kind: "noEntries",
    });
    expect(logsEmptyState({ loading: false, filtered: true, warning: undefined })).toEqual({
      kind: "noMatch",
    });
  });

  it("shows loading before the first answer", () => {
    expect(logsEmptyState({ loading: true, filtered: false, warning: undefined })).toEqual({
      kind: "loading",
    });
  });
});
