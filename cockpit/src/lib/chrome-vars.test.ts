import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { CHROME } from "@/lib/cockpit-layout";

const CSS = readFileSync(path.resolve(__dirname, "../styles/globals.css"), "utf8");

describe("the framed screens' shell chrome variables", () => {
  const used = new Set(Object.values(CHROME).flatMap((cls) => [...cls.matchAll(/var\((--[a-z-]+)\)/g)].map((m) => m[1])));

  it("names exactly the strip, bar, rail and menu strip", () => {
    expect([...used].sort()).toEqual(["--chrome-bottom", "--chrome-rail", "--chrome-strip", "--chrome-top"]);
  });

  it("declares every variable the chrome uses", () => {
    for (const name of used) expect(CSS).toMatch(new RegExp(`${name}:`));
  });

  it("grows the touch-target bars with glove mode", () => {
    expect(CSS).toMatch(/--chrome-bottom: calc\(3rem \* var\(--touch-scale\)\)/);
    expect(CSS).toMatch(/--chrome-strip: calc\(3\.6rem \* var\(--touch-scale\)\)/);
  });
});
