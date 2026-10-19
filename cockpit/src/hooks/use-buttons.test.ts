import { describe, expect, it } from "vitest";

import { eventToCommand } from "./use-buttons";

describe("eventToCommand", () => {
  it("maps a bound button", () => {
    expect(eventToCommand({ button: "17", label: "b3", kind: "short", action: "cycle_screen" })).toBe(
      "activate",
    );
  });

  it("leaves a passthrough button to plugins", () => {
    expect(eventToCommand({ button: "5", label: "b1", kind: "short", action: "passthrough" })).toBeNull();
    expect(eventToCommand({ button: "5", label: "b1", kind: "long", action: "passthrough" })).toBeNull();
  });
});
