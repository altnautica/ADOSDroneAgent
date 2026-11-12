// Every form that stages a draft registers it with the dirty guard, so leaving
// the screen asks before the draft is dropped.

import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const SRC = path.resolve(__dirname, "../..");

const DRAFT_FORMS = [
  "components/screens/transmit-screen.tsx",
  "components/screens/video-screen.tsx",
  "components/screens/params-screen.tsx",
  "components/screens/msp-screen.tsx",
  "components/settings/controls/controls-settings.tsx",
  "components/settings/leaf-editor.tsx",
  "components/settings/sections/cellular-section.tsx",
  "components/settings/sections/capture-section.tsx",
  "components/settings/sections/uplink-section.tsx",
  "components/settings/sections/security-section.tsx",
];

describe("draft forms", () => {
  it.each(DRAFT_FORMS)("%s registers its draft with the dirty guard", (file) => {
    const src = readFileSync(path.join(SRC, file), "utf8");
    expect(src).toMatch(/\buse(Dirty|Draft)Guard\(/);
  });
});
