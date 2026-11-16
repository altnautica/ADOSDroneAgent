import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { parsePilotState } from "./cockpit-frames";
import { noRcChipText, noRcStripText } from "./no-rc";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const frame = (extra: Record<string, unknown>) =>
  parsePilotState({ ...JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")), ...extra })!;

describe("the no-RC posture", () => {
  it("shows the chip and the line with the reported action and window", () => {
    const p = frame({ rc_receiver: "absent", link_loss_action: "land", link_loss_s: 8 });
    expect(noRcChipText(p)).toBe("No controller");
    expect(noRcStripText(p)).toBe("No controller · Stop it from this phone: Pause, Land or Return · If the link drops for 8 s: Land");
  });

  it("falls back to the pilot's defaults from an agent that omits them", () => {
    const p = frame({ rc_receiver: "absent", link_loss_action: undefined, link_loss_s: undefined });
    expect(noRcStripText(p)).toBe("No controller · Stop it from this phone: Pause, Land or Return · If the link drops for 10 s: Return home");
  });

  it("is absent with an RC transmitter or an agent that omits the field", () => {
    for (const p of [frame({ rc_receiver: "present" }), frame({})]) {
      expect(noRcChipText(p)).toBeNull();
      expect(noRcStripText(p)).toBeNull();
    }
  });
});
