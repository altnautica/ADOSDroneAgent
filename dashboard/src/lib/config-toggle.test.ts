import { describe, expect, it } from "vitest";

import { toggleView } from "@/lib/config-toggle";

describe("config switch view", () => {
  it("draws an unread value as unknown and refuses a press", () => {
    expect(toggleView(undefined, false, false)).toEqual({ unknown: true, checked: false, disabled: true });
  });

  it("is disabled while a write is in flight", () => {
    expect(toggleView(false, true, false).disabled).toBe(true);
  });

  it("draws a read value as it is", () => {
    expect(toggleView(true, false, false)).toEqual({ unknown: false, checked: true, disabled: false });
  });
});
