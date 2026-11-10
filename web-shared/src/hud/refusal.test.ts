import { describe, expect, it } from "vitest";

import refusalCases from "./fixtures/refusal-cases.json";
import { refusalText } from "./refusal";

describe("refusal text", () => {
  it("names the pilot's refusals in words, sentence case", () => {
    expect(refusalText("no_position")).toBe("No position from the flight controller");
    expect(refusalText("failsafe")).toBe("The drone is in a failsafe return or landing; change mode first");
    expect(refusalText("landed")).toBe("On the ground");
    expect(refusalText("no_home")).toMatch(/home/);
    expect(refusalText("launching")).toMatch(/climb/);
    expect(refusalText("control_mode")).toMatch(/flight controller/);
  });

  it("never shows a built-in refusal code raw", () => {
    for (const code of [
      "disarmed",
      "not_armed",
      "busy",
      "tuning",
      "nothing_paused",
      "not_landed",
      "vehicle_class",
      "no_fc",
      "unknown_action",
      "bad_args",
      "failsafe",
    ]) {
      expect(refusalText(code)).not.toBe(code);
      expect(refusalText(code)).not.toMatch(/_|^Refused/);
    }
  });

  it("maps the client's own codes with spaces through underscores", () => {
    expect(refusalText("no response")).toBe("No answer from the drone");
    expect(refusalText("no pilot link")).toBe("No answer from the drone");
    expect(refusalText("closed")).toBe("The link to the drone closed");
  });

  it("names the readiness item behind not_ready", () => {
    expect(refusalText("not_ready:position")).toBe("Not ready: Position estimate");
    expect(refusalText("not_ready:position", (id) => `item ${id}`)).toBe("Not ready: item position");
    expect(refusalText("not_ready:something_new")).toBe("Not ready: something_new");
  });

  it("shows an unknown code and says Refused for none", () => {
    expect(refusalText("something_new")).toBe("Refused: something_new");
    expect(refusalText(undefined)).toBe("Refused");
    expect(refusalText(null)).toBe("Refused");
    expect(refusalText("")).toBe("Refused");
  });
});

describe("refusal golden cases (fixtures/refusal-cases.json)", () => {
  it("has a case per wire code, the client's own codes, an unknown and none", () => {
    expect(refusalCases.length).toBeGreaterThanOrEqual(55);
    expect(refusalCases.map((c) => c.input.code)).toEqual(expect.arrayContaining(["not_ready:position", "no response", "no pilot link", "closed", "", null]));
  });

  for (const c of refusalCases) {
    it(c.name, () => {
      expect(refusalText(c.input.code)).toBe(c.expected.text);
    });
  }
});
