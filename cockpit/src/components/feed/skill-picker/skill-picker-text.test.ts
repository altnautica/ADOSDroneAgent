// The skill picker and the controller cell draw their words from the cockpit
// spec and their colours from its tokens, never from literals of their own.

import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { padStatusText, type PadStatus } from "@/hooks/use-gamepad";
import { PICKER_TABS } from "@/lib/skill-picker-model";

const read = (rel: string) => readFileSync(path.resolve(__dirname, rel), "utf8");

describe("skill picker text", () => {
  it("has no hard-coded flight-screen words or colours", () => {
    const src = read("skill-picker.tsx") + read("../../../lib/skill-picker-model.ts");
    for (const literal of ['"GO"', '"Starting…"', '"ACTIVE', "`ACTIVE", '"Skills"', '"Stop"', '"On"', '"Off"', '"Behaviors"', '"CLEAR"', "No skills in this tab", "#22D3EE", "last seen"]) {
      expect(src, literal).not.toContain(literal);
    }
  });

  it("labels the tabs from the spec", () => {
    expect(PICKER_TABS.map((t) => t.label)).toEqual(["Behaviors", "Camera", "Navigation", "Utility"]);
  });
});

describe("controller cell text", () => {
  const pad = (over: Partial<PadStatus>): PadStatus => ({ connected: true, name: null, standard: true, batteryPct: null, native: false, ...over });

  it("reads the spec's controls.pad words", () => {
    expect(padStatusText(pad({ connected: false }))).toEqual({ text: "No pad", ok: null });
    expect(padStatusText(pad({ native: true, batteryPct: 81.6 }))).toEqual({ text: "Native 82%", ok: true });
    expect(padStatusText(pad({}))).toEqual({ text: "Pad", ok: true });
    expect(padStatusText(pad({ batteryPct: 40 }))).toEqual({ text: "Pad 40%", ok: true });
    expect(padStatusText(pad({ standard: false }))).toEqual({ text: "Not standard", ok: false });
  });
});
