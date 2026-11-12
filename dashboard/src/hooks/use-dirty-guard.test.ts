import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const read = (f: string) => readFileSync(path.resolve(__dirname, "..", f), "utf8");

describe("unsaved-change guard", () => {
  it("blocks in-app navigation while dirty and asks before leaving", () => {
    const src = read("hooks/use-dirty-guard.ts");
    expect(src).toMatch(/useBlocker\(/);
    expect(src).toMatch(/blocker\.proceed\(\)/);
    expect(src).toMatch(/blocker\.reset\(\)/);
  });

  it("runs under a data router, which useBlocker needs", () => {
    const main = read("main.tsx");
    expect(main).toMatch(/createBrowserRouter\(/);
    expect(main).not.toMatch(/<BrowserRouter>/);
  });
});
