import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { CHROME } from "@/lib/cockpit-layout";

const CSS = readFileSync(path.resolve(__dirname, "../styles/globals.css"), "utf8");

describe("the shell chrome variables", () => {
  const used = new Set(Object.values(CHROME).flatMap((cls) => [...cls.matchAll(/var\((--[a-z-]+)\)/g)].map((m) => m[1])));

  it("names exactly the strip, bar, rail, menu strip and gap", () => {
    expect([...used].sort()).toEqual(["--chrome-bottom", "--chrome-gap", "--chrome-rail", "--chrome-strip", "--chrome-top"]);
  });

  it("declares every variable the chrome and the Feed region use", () => {
    for (const name of used) expect(CSS).toMatch(new RegExp(`${name}:`));
  });

  it("sizes the chrome and the Feed region from the same variables", () => {
    expect(CHROME.stripHeight).toContain("--chrome-top");
    expect(CHROME.regionTop).toContain("--chrome-top");
    expect(CHROME.barHeight).toContain("--chrome-bottom");
    expect(CHROME.regionBottom).toContain("--chrome-bottom");
    expect(CHROME.menuSize).toContain("--chrome-rail");
    expect(CHROME.regionLeftBesideRail).toContain("--chrome-rail");
  });

  it("grows the touch-target bars with glove mode", () => {
    expect(CSS).toMatch(/--chrome-bottom: calc\(3rem \* var\(--touch-scale\)\)/);
    expect(CSS).toMatch(/--chrome-strip: calc\(3\.6rem \* var\(--touch-scale\)\)/);
  });
});
