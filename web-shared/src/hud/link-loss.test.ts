import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { parsePilotState } from "./cockpit-frames";
import { linkLossLine } from "./link-loss";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const withWire = (extra: Record<string, unknown>) =>
  parsePilotState({ ...JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")), link_loss_in_s: 4.2, ...extra })!;

describe("link-loss countdown", () => {
  it("names the action the agent reports", () => {
    expect(linkLossLine(withWire({ link_loss_action: "land" }))).toBe("Link lost: Land in 5 s");
    expect(linkLossLine(withWire({ link_loss_action: "hold" }))).toBe("Link lost: Hold in 5 s");
    expect(linkLossLine(withWire({ link_loss_action: "rth" }))).toBe("Link lost: Return home in 5 s");
  });

  it("reads the flight controller's names for the same actions", () => {
    expect(linkLossLine(withWire({ link_loss_action: "rtl" }))).toBe("Link lost: Return home in 5 s");
    expect(linkLossLine(withWire({ link_loss_action: "loiter" }))).toBe("Link lost: Hold in 5 s");
  });

  it("never guesses the action, and is null with no countdown", () => {
    expect(linkLossLine(withWire({ link_loss_action: undefined }))).toBe("Link lost: — in 5 s");
    expect(linkLossLine(withWire({ link_loss_action: "spin" }))).toBe("Link lost: — in 5 s");
    expect(linkLossLine({ linkLossInS: 3, linkLossAction: "spin" })).toBe("Link lost: — in 3 s");
    expect(linkLossLine(withWire({ link_loss_in_s: null, link_loss_action: "land" }))).toBeNull();
  });
});
