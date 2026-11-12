import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { stagedChanges } from "./param-write-list";

describe("parameter write confirm", () => {
  it("names each staged write old → new and marks a value out of range", () => {
    const drafts = new Map([
      ["RTL_ALT", { original: 1500, draft: 99999 }],
      ["ARMING_CHECK", { original: 1, draft: 0 }],
    ]);
    const meta = new Map([["RTL_ALT", { range: { min: 200, max: 30000 } }]]) as never;
    const { shown, more } = stagedChanges(drafts, meta);
    expect(shown).toEqual([
      { name: "ARMING_CHECK", from: 1, to: 0, outOfRange: false },
      { name: "RTL_ALT", from: 1500, to: 99999, outOfRange: true },
    ]);
    expect(more).toBe(0);
  });

  it("shows the first ten and counts the rest", () => {
    const drafts = new Map(Array.from({ length: 13 }, (_, i) => [`P${String(i).padStart(2, "0")}`, { original: 0, draft: 1 }]));
    const { shown, more } = stagedChanges(drafts, undefined);
    expect(shown).toHaveLength(10);
    expect(more).toBe(3);
  });

  it("the confirm renders the list", () => {
    const src = readFileSync(path.resolve(__dirname, "mavlink-params.tsx"), "utf8");
    expect(src).toMatch(/<StagedList drafts=\{drafts\} meta=\{meta\} \/>/);
  });
});
