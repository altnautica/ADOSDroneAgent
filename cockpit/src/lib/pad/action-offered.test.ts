import { describe, expect, it } from "vitest";

import { actionOffered } from "@/lib/pad/bindings";

describe("the emergency stop in the controls editor", () => {
  it("is offered only in the Hold cell", () => {
    expect(actionOffered("kill", "hold", "none")).toBe(true);
    for (const g of ["press", "tap", "double_tap"]) expect(actionOffered("kill", g, "none")).toBe(false);
  });

  it("stays visible on a row that already is a stop, and other actions stay everywhere", () => {
    expect(actionOffered("kill", "tap", "kill")).toBe(true);
    expect(actionOffered("rth", "tap", "none")).toBe(true);
  });
});
